use std::thread;

use alloy_primitives::{Address, hex};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope};
use base_consensus_gossip::{BlockHandler, Handler, MAX_GOSSIP_SIZE, P2pRpcRequest};
use base_consensus_sources::{BlockSignerHandler, BlockSignerStartError};
use base_tiger_p2p::{Config, Event, Handle, Network, Validation};
use libp2p::gossipsub::{IdentTopic, Message, MessageAcceptance, TopicHash};
use secp256k1_tp2p::{PublicKey, SecretKey};
use tokio::sync::{mpsc, watch};

use crate::actors::network::{
    NetworkActorError,
    tp2p::{Tp2pConfig, Tp2pPeer},
    transport::GossipTransport,
};

/// Why the tp2p transport failed to start.
#[derive(Debug, thiserror::Error)]
pub enum Tp2pStartError {
    /// The configured secret is not a valid secp256k1 secret key.
    #[error("invalid tp2p secret key")]
    InvalidSecretKey,
    /// Binding the listener or spawning a thread failed.
    #[error("failed to start tp2p: {0}")]
    Io(#[from] std::io::Error),
    /// Starting the block signer failed.
    #[error("failed to start the tp2p block signer: {0}")]
    Signer(#[from] BlockSignerStartError),
}

/// Gossips unsafe payloads over tp2p.
///
/// A tp2p gossip message is `(topic, data)`, where the topic is the payload version `v` (0 to 3)
/// and the data is exactly what libp2p publishes on `/optimism/{chain_id}/{v}/blocks`. Inbound
/// messages go through this transport's own [`BlockHandler`] under that topic, so they pass the
/// same checks as libp2p gossip, and its verdict is tp2p's validation verdict.
///
/// tp2p runs on std threads. A bridge thread forwards its events into a bounded tokio channel;
/// dropping the transport stops tp2p and joins the bridge.
#[derive(Debug)]
pub struct Tp2pTransport {
    /// The running tp2p node.
    pub handle: Handle,
    /// tp2p events, forwarded by the bridge thread.
    pub events: mpsc::Receiver<Event>,
    /// Validates inbound gossip, with its own seen-block state.
    pub handler: BlockHandler,
    /// Updates the unsafe block signer that [`Self::handler`] expects.
    pub unsafe_block_signer_sender: watch::Sender<Address>,
    /// Signs payloads this node publishes; none on a node that does not sequence or that
    /// publishes through another transport's signer.
    pub signer: Option<BlockSignerHandler>,
    bridge: Option<thread::JoinHandle<()>>,
}

impl Tp2pTransport {
    /// tp2p events buffered between the bridge thread and the network actor.
    pub const EVENT_BUFFER: usize = 1024;

    /// Starts tp2p on the consensus network of `rollup_config`'s chain and genesis.
    pub fn start(
        rollup_config: RollupConfig,
        unsafe_block_signer: Address,
        config: &Tp2pConfig,
        signer: Option<BlockSignerHandler>,
    ) -> Result<Self, Tp2pStartError> {
        let secret = SecretKey::from_byte_array(config.secret.0)
            .map_err(|_| Tp2pStartError::InvalidSecretKey)?;
        let network = Network {
            chain_id: rollup_config.l2_chain_id.id(),
            genesis: rollup_config.genesis.l2.hash.0,
            layer: Network::CONSENSUS,
        };
        let mut p2p = Config::new(network, secret, 0);
        p2p.advertise = config.advertise;
        p2p.max_gossip_len = MAX_GOSSIP_SIZE;
        p2p.bootnodes = config.bootnodes.iter().map(|peer| (peer.key, peer.addr)).collect();
        for peer in &config.trusted {
            if !p2p.bootnodes.contains(&(peer.key, peer.addr)) {
                p2p.bootnodes.push((peer.key, peer.addr));
            }
        }
        p2p.trusted = config.trusted.iter().map(|peer| peer.key).collect();
        p2p.peers_file = config.peers_file.clone();

        let (handle, std_events) = base_tiger_p2p::start(p2p, Some(config.listen), false)?;
        let address = Tp2pPeer {
            key: PublicKey::from_secret_key_global(&secret),
            addr: config.advertise.or_else(|| handle.local_addr()).unwrap_or(config.listen),
        };
        info!(target: "net::tp2p", address = %address, "Started tp2p");

        let (events_tx, events) = mpsc::channel(Self::EVENT_BUFFER);
        let bridge = thread::Builder::new().name("tp2p-events".into()).spawn(move || {
            // Ends when tp2p stops (its sender drops) or the transport drops (the send fails).
            while let Ok(event) = std_events.recv() {
                if events_tx.blocking_send(event).is_err() {
                    break;
                }
            }
        });
        let bridge = match bridge {
            Ok(bridge) => bridge,
            Err(error) => {
                handle.shutdown();
                return Err(error.into());
            }
        };

        let (unsafe_block_signer_sender, signer_recv) = watch::channel(unsafe_block_signer);
        Ok(Self {
            handle,
            events,
            handler: BlockHandler::new(rollup_config, signer_recv),
            unsafe_block_signer_sender,
            signer,
            bridge: Some(bridge),
        })
    }

    /// The handler's block topics, indexed by tp2p topic (payload version 0 to 3).
    pub const fn block_topics(&self) -> [&IdentTopic; 4] {
        [
            &self.handler.blocks_v1_topic,
            &self.handler.blocks_v2_topic,
            &self.handler.blocks_v3_topic,
            &self.handler.blocks_v4_topic,
        ]
    }

    /// The libp2p topic for a tp2p topic: `/optimism/{chain_id}/{topic}/blocks`, which the handler
    /// rejects as unknown above 3.
    pub fn topic_hash(&self, topic: u8) -> TopicHash {
        self.block_topics().get(usize::from(topic)).map_or_else(
            || {
                let chain_id = self.handler.rollup_config.l2_chain_id.id();
                IdentTopic::new(format!("/optimism/{chain_id}/{topic}/blocks")).hash()
            },
            |known| known.hash(),
        )
    }

    /// Validates a tp2p gossip message with [`BlockHandler`], as libp2p gossip on the same topic.
    pub fn validate(
        &mut self,
        topic: u8,
        data: Vec<u8>,
    ) -> (MessageAcceptance, Option<NetworkPayloadEnvelope>) {
        let topic = self.topic_hash(topic);
        self.handler.handle(Message { source: None, data, sequence_number: None, topic })
    }

    /// Publishes an already signed payload with the topic and encoding libp2p uses for it.
    pub async fn publish_signed(&self, payload: NetworkPayloadEnvelope) {
        let topic = self.handler.topic(payload.payload.timestamp());
        let Some(index) = self.block_topics().iter().position(|known| known.hash() == topic.hash())
        else {
            warn!(target: "net::tp2p", topic = %topic, "No tp2p topic for unsafe payload");
            return;
        };
        let data = match self.handler.encode(topic, payload) {
            Ok(data) => data,
            Err(error) => {
                warn!(target: "net::tp2p", error = ?error, "Failed to encode unsafe payload");
                return;
            }
        };
        // Publishing waits for tp2p's core thread to assign the message id.
        let handle = self.handle.clone();
        match tokio::task::spawn_blocking(move || handle.publish(index as u8, data)).await {
            Ok(id) => info!(target: "net::tp2p", id = %hex::encode(id), "Published unsafe payload"),
            Err(error) => {
                warn!(target: "net::tp2p", error = %error, "Failed to publish unsafe payload")
            }
        }
    }
}

impl Drop for Tp2pTransport {
    fn drop(&mut self) {
        // Closing the channel unblocks a bridge waiting for capacity; stopping tp2p drops the
        // sender the bridge reads from.
        self.events.close();
        self.handle.shutdown();
        if let Some(bridge) = self.bridge.take()
            && bridge.join().is_err()
        {
            warn!(target: "net::tp2p", "tp2p event bridge panicked");
        }
    }
}

#[async_trait]
impl GossipTransport for Tp2pTransport {
    type Error = NetworkActorError;

    async fn publish(&mut self, block: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        let Some(signer) = self.signer.as_ref() else {
            warn!(target: "net::tp2p", "No local signer available to sign the payload");
            return Ok(());
        };
        let chain_id = self.handler.rollup_config.l2_chain_id.id();
        let sender_address = *self.unsafe_block_signer_sender.borrow();
        let payload = signer.sign_payload(block, chain_id, sender_address).await?;
        self.publish_signed(payload).await;
        Ok(())
    }

    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        loop {
            match self.events.recv().await? {
                Event::Gossip { id, topic, data, from } => {
                    let (acceptance, payload) = self.validate(topic, data);
                    let validation = match acceptance {
                        MessageAcceptance::Accept => Validation::Accept,
                        MessageAcceptance::Ignore => Validation::Ignore,
                        MessageAcceptance::Reject => Validation::Reject,
                    };
                    self.handle.validated(id, validation);
                    if validation == Validation::Reject {
                        debug!(target: "net::tp2p", peer = %hex::encode(from), topic, "Rejected gossip");
                    }
                    if payload.is_some() {
                        return payload;
                    }
                }
                Event::PeerReady { peer, .. } => {
                    debug!(target: "net::tp2p", peer = %hex::encode(peer), "Peer ready");
                }
                Event::PeerGone { peer } => {
                    debug!(target: "net::tp2p", peer = %hex::encode(peer), "Peer gone");
                }
                Event::Request { .. } | Event::Response { .. } | Event::Notify { .. } => {
                    trace!(target: "net::tp2p", "Ignoring event outside consensus gossip");
                }
            }
        }
    }

    fn set_block_signer(&mut self, address: Address) {
        if self.unsafe_block_signer_sender.send(address).is_err() {
            warn!(target: "net::tp2p", "Failed to update unsafe block signer address");
        }
    }

    fn clear_pending_connections(&mut self) -> usize {
        0
    }

    fn handle_p2p_rpc(&mut self, _request: P2pRpcRequest) {
        // The P2P RPC API describes libp2p and discv5 state; dropping the request closes its
        // reply channel, so the caller gets an error.
        debug!(target: "net::tp2p", "P2P RPC is not served over tp2p");
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use alloy_chains::Chain;
    use alloy_consensus::{Block, Header};
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_rpc_types_engine::BaseExecutionPayload;
    use tokio::time::timeout;

    use super::*;
    use crate::Tp2pMode;

    const CHAIN_ID: u64 = 8453;
    const WAIT: Duration = Duration::from_secs(10);

    fn rollup_config() -> RollupConfig {
        RollupConfig { l2_chain_id: Chain::from_id(CHAIN_ID), ..Default::default() }
    }

    /// A pre-Canyon payload stamped now, so it is valid on the v1 topic.
    fn payload(number: u64) -> BaseExecutionPayloadEnvelope {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let block = Block::<BaseTxEnvelope> {
            header: Header { number, timestamp, base_fee_per_gas: Some(1), ..Default::default() },
            body: Default::default(),
        };
        BaseExecutionPayloadEnvelope {
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1::from_block_slow(
                &block,
            )),
            parent_beacon_block_root: None,
        }
    }

    fn config(secret: [u8; 32], bootnodes: Vec<Tp2pPeer>) -> Tp2pConfig {
        Tp2pConfig {
            mode: Tp2pMode::Only,
            listen: "127.0.0.1:0".parse().unwrap(),
            advertise: None,
            bootnodes,
            trusted: Vec::new(),
            secret: B256::from(secret),
            peers_file: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delivers_sequencer_payloads_and_rejects_forged_ones() {
        let sequencer = PrivateKeySigner::random();
        let mut publisher = Tp2pTransport::start(
            rollup_config(),
            sequencer.address(),
            &config([1; 32], Vec::new()),
            Some(BlockSignerHandler::Local(sequencer.clone())),
        )
        .unwrap();
        let publisher_peer = Tp2pPeer {
            key: PublicKey::from_secret_key_global(&SecretKey::from_byte_array([1; 32]).unwrap()),
            addr: publisher.handle.local_addr().unwrap(),
        };
        let mut follower = Tp2pTransport::start(
            rollup_config(),
            sequencer.address(),
            &config([2; 32], vec![publisher_peer]),
            None,
        )
        .unwrap();

        timeout(WAIT, async {
            while publisher.handle.peers().is_empty() || follower.handle.peers().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("peers connect");

        // Signed by the expected sequencer key: delivered with the libp2p encoding's contents.
        let signed = payload(1);
        let block_hash = signed.execution_payload.block_hash();
        publisher.publish(signed).await.unwrap();
        let received = timeout(WAIT, follower.next_unsafe_block()).await.unwrap().unwrap();
        assert_eq!(received.payload.block_hash(), block_hash);
        let signer = received
            .signature
            .recover_address_from_prehash(&received.payload_hash.signature_message(CHAIN_ID))
            .unwrap();
        assert_eq!(signer, sequencer.address());

        // Signed by another key: rejected, not delivered, and the sender is penalized.
        let peer_id = publisher_peer.key.serialize();
        assert_eq!(follower.handle.score(peer_id), Some(0));
        let forged = BlockSignerHandler::Local(PrivateKeySigner::random())
            .sign_payload(payload(2), CHAIN_ID, Address::ZERO)
            .await
            .unwrap();
        publisher.publish_signed(forged).await;
        assert!(timeout(Duration::from_secs(2), follower.next_unsafe_block()).await.is_err());
        assert!(follower.handle.score(peer_id).unwrap() < 0);
    }
}
