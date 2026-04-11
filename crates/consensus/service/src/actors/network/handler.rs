use std::collections::HashSet;

use alloy_primitives::Address;
use async_trait::async_trait;
use base_alloy_rpc_types_engine::{
    BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_disc::{Discv5Handler, HandlerRequest};
use base_consensus_gossip::{
    BlockHandler, ConnectionGate, ConnectionGater, GossipDriver, Metrics, P2pRpcRequest,
};
use base_consensus_sources::BlockSignerHandler;
use discv5::Enr;
use tokio::{
    select,
    sync::{mpsc, watch},
};

use crate::actors::network::{NetworkActorError, transport::GossipTransport};

/// A network handler used to communicate with the network once it is started.
#[derive(Debug)]
pub struct NetworkHandler {
    /// The gossip driver.
    pub gossip: GossipDriver<ConnectionGater>,
    /// The discovery handler.
    pub discovery: Discv5Handler,
    /// The receiver for the ENRs.
    pub enr_receiver: mpsc::Receiver<Enr>,
    /// The sender for the unsafe block signer.
    pub unsafe_block_signer_sender: watch::Sender<Address>,
    /// The peer score inspector. Is used to ban peers that are below a given threshold.
    pub peer_score_inspector: tokio::time::Interval,
    /// A handler for the block signer.
    pub signer: Option<BlockSignerHandler>,
}

impl NetworkHandler {
    pub(super) async fn handle_peer_monitoring(&mut self) {
        // Inspect peer scores and ban peers that are below the threshold.
        let Some(ban_peers) = self.gossip.peer_monitoring.as_ref() else {
            return;
        };

        // We iterate over all connected peers and check their scores.
        // We collect a list of peers to remove
        let peers_to_remove = self
            .gossip
            .swarm
            .connected_peers()
            .filter_map(|peer_id| {
                // If the score is not available, we use a default value of 0.
                let score =
                    self.gossip.swarm.behaviour().gossipsub.peer_score(peer_id).unwrap_or_default();

                // Record the peer score in the metrics.
                Metrics::peer_scores().record(score);

                if score < ban_peers.ban_threshold {
                    return Some(*peer_id);
                }

                None
            })
            .collect::<Vec<_>>();

        // We remove the addresses from the gossip layer.
        let addrs_to_ban = peers_to_remove
            .into_iter()
            .filter_map(|peer_to_remove| {
                // In that case, we ban the peer. This means...
                // 1. We remove the peer from the network gossip.
                // 2. We ban the peer from the discv5 service.
                if self.gossip.swarm.disconnect_peer_id(peer_to_remove).is_err() {
                    warn!(peer = ?peer_to_remove, "Trying to disconnect a non-existing peer from the gossip driver.");
                }

                        // Record the duration of the peer connection.
                        if let Some(start_time) = self.gossip.peer_connection_start.remove(&peer_to_remove) {
                            Metrics::gossip_peer_connection_duration_seconds()
                                .record(start_time.elapsed().as_secs_f64());
                        }

                if let Some(info) = self.gossip.peerstore.remove(&peer_to_remove) {
                    self.gossip.connection_gate.remove_dial(&peer_to_remove);
                    let _score = self.gossip.swarm.behaviour().gossipsub.peer_score(&peer_to_remove).unwrap_or_default();
                    Metrics::banned_peers().increment(1.0);
                    return Some(info.listen_addrs);
                }

                None
            })
            .flatten()
            .collect::<HashSet<_>>();

        // We send a request to the discovery handler to ban the set of addresses.
        if let Err(send_err) = self
            .discovery
            .sender
            .send(HandlerRequest::BanAddrs {
                addrs_to_ban: addrs_to_ban.into(),
                ban_duration: ban_peers.ban_duration,
            })
            .await
        {
            warn!(err = ?send_err, "Impossible to send a request to the discovery handler. The channel connection is dropped.");
        }
    }
}

#[async_trait]
impl GossipTransport for NetworkHandler {
    type Error = NetworkActorError;

    async fn publish(&mut self, block: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        let timestamp = block.execution_payload.timestamp();
        let selector = |handler: &BlockHandler| handler.topic(timestamp);

        let Some(signer) = self.signer.as_ref() else {
            warn!(target: "net", "No local signer available to sign the payload");
            return Ok(());
        };

        let chain_id = self.discovery.chain_id;
        let sender_address = *self.unsafe_block_signer_sender.borrow();
        let payload_hash: PayloadHash = block.payload_hash();
        let signature = signer.sign_block(payload_hash, chain_id, sender_address).await?;

        let payload = NetworkPayloadEnvelope {
            payload: block.execution_payload,
            parent_beacon_block_root: block.parent_beacon_block_root,
            signature,
            payload_hash,
        };

        match self.gossip.publish(selector, Some(payload)) {
            Ok(id) => info!(id = ?id, "Published unsafe payload"),
            Err(e) => warn!(error = ?e, "Failed to publish unsafe payload"),
        }

        Ok(())
    }

    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        loop {
            let has_peer_monitoring = self.gossip.peer_monitoring.as_ref().is_some();
            select! {
                event = self.gossip.next() => {
                    let event = event?;
                    if let Some(payload) = self.gossip.handle_event(event) {
                        return Some(payload);
                    }
                }
                enr = self.enr_receiver.recv() => {
                    let enr = enr?;
                    self.gossip.dial(enr);
                }
                _ = self.peer_score_inspector.tick(), if has_peer_monitoring => {
                    self.handle_peer_monitoring().await;
                }
            }
        }
    }

    fn set_block_signer(&mut self, address: Address) {
        if self.unsafe_block_signer_sender.send(address).is_err() {
            warn!(target: "network", "Failed to update unsafe block signer address");
        }
    }

    fn handle_p2p_rpc(&mut self, request: P2pRpcRequest) {
        request.handle(&mut self.gossip, &self.discovery);
    }
}
