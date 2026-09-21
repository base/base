//! Consensus-layer gossipsub driver for Base.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::Address;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::NetworkPayloadEnvelope;
use base_consensus_peers::{EnrValidation, PeerMonitoring, PeerUtils};
use derive_more::Debug;
use discv5::Enr;
use futures::stream::StreamExt;
use libp2p::{
    Multiaddr, PeerId, Swarm, TransportError,
    gossipsub::{IdentTopic, MessageId},
    swarm::{
        SwarmEvent,
        dial_opts::{DialOpts, PeerCondition},
    },
};
use libp2p_identity::Keypair;
use lru::LruCache;
#[cfg(test)]
pub use tests::GossipPair;
use tokio::{
    sync::Mutex,
    time::{Instant as TokioInstant, sleep_until},
};

use crate::{
    Behaviour, BlockHandler, ConnectionGate, ConnectionGater, ConnectionLimitsConfig, Event,
    GossipDriverBuilder, Handler, HandlerEncodeError, Metrics, PublishError,
};

/// Configuration applied when constructing a [`GossipDriver`].
#[derive(Debug, Clone)]
pub struct GossipDriverConfig {
    /// Maximum number of peers to retain identify metadata for.
    pub max_identify_peerstore_peers: NonZeroUsize,
    /// Peer score monitoring config.
    pub peer_monitoring: Option<PeerMonitoring>,
    /// The configured libp2p connection limits enforced by the swarm.
    pub connection_limits_config: ConnectionLimitsConfig,
}

/// A driver for a [`Swarm`] instance.
///
/// Connects the swarm to the given [`Multiaddr`]
/// and handles events using the [`BlockHandler`].
#[derive(Debug)]
pub struct GossipDriver<G: ConnectionGate> {
    /// The [`Swarm`] instance.
    #[debug(skip)]
    pub swarm: Swarm<Behaviour>,
    /// A [`Multiaddr`] to listen on.
    pub addr: Multiaddr,
    /// The [`BlockHandler`].
    pub handler: BlockHandler,
    /// LRU cache of identify metadata keyed by [`PeerId`].
    pub peerstore: LruCache<PeerId, libp2p::identify::Info>,
    /// If set, the gossip layer will monitor peer scores and ban peers that are below a given
    /// threshold.
    pub peer_monitoring: Option<PeerMonitoring>,
    /// Tracks connection start time for peers
    pub peer_connection_start: HashMap<PeerId, Instant>,
    /// The connection gate.
    pub connection_gate: G,
    /// The configured libp2p connection limits enforced by the swarm.
    pub connection_limits_config: ConnectionLimitsConfig,
    /// Tracks ping times for peers.
    pub ping: Arc<Mutex<HashMap<PeerId, Duration>>>,
    /// Next check for obsolete block topics. Kept across polls so busy traffic
    /// or cancellation of `next` cannot indefinitely postpone retirement.
    pub next_topic_retirement: TokioInstant,
}

impl<G> GossipDriver<G>
where
    G: ConnectionGate,
{
    /// How often to reconcile subscriptions with the configured fork schedule.
    /// The handler enforces retirement before decoding even between checks.
    pub const TOPIC_RETIREMENT_INTERVAL: Duration = Duration::from_secs(5);

    /// Returns the [`GossipDriverBuilder`] that can be used to construct the [`GossipDriver`].
    pub const fn builder(
        rollup_config: RollupConfig,
        signer: Address,
        gossip_addr: Multiaddr,
        keypair: Keypair,
    ) -> GossipDriverBuilder {
        GossipDriverBuilder::new(rollup_config, signer, gossip_addr, keypair)
    }

    /// Creates a new [`GossipDriver`] instance.
    pub fn new(
        swarm: Swarm<Behaviour>,
        addr: Multiaddr,
        handler: BlockHandler,
        gate: G,
        config: GossipDriverConfig,
    ) -> Self {
        let driver = Self {
            swarm,
            addr,
            handler,
            peerstore: LruCache::new(config.max_identify_peerstore_peers),
            peer_monitoring: config.peer_monitoring,
            peer_connection_start: Default::default(),
            connection_gate: gate,
            connection_limits_config: config.connection_limits_config,
            ping: Arc::new(Mutex::new(Default::default())),
            next_topic_retirement: TokioInstant::now(),
        };
        #[cfg(feature = "metrics")]
        driver.record_block_topic_metrics();
        driver
    }

    /// Publishes an unsafe block to gossip.
    ///
    /// ## Arguments
    ///
    /// * `topic_selector` - A function that selects the topic for the block. This is expected to be
    ///   a closure that takes the [`BlockHandler`] and returns the [`IdentTopic`] for the block.
    /// * `payload` - The payload to be published.
    ///
    /// ## Returns
    ///
    /// Returns the [`MessageId`] of the published message or a [`PublishError`]
    /// if the message could not be published.
    pub fn publish(
        &mut self,
        selector: impl FnOnce(&BlockHandler) -> IdentTopic,
        payload: Option<NetworkPayloadEnvelope>,
    ) -> Result<Option<MessageId>, PublishError> {
        let Some(payload) = payload else {
            return Ok(None);
        };
        let topic = selector(&self.handler);
        let topic_hash = topic.hash();
        // GossipSub can publish to unsubscribed topics. Do not let a retired
        // topic re-enter the publishing path, including after a clock rollback.
        // The handler's clock check additionally enforces the cutoff before
        // the next periodic sweep removes the subscription.
        if !self.swarm.behaviour().gossipsub.topics().any(|topic| *topic == topic_hash) {
            if let Some(version) = self.handler.topic_version(&topic_hash) {
                Metrics::block_topic_blocked_total(version, "outbound").increment(1);
            }
            return Err(HandlerEncodeError::UnknownTopic(topic_hash).into());
        }
        let data = self.handler.encode(topic, payload)?;
        let id = self.swarm.behaviour_mut().gossipsub.publish(topic_hash, data)?;
        Metrics::unsafe_block_published().increment(1.0);
        Ok(Some(id))
    }

    /// Starts the libp2p swarm listening on the configured [`Multiaddr`].
    ///
    /// Waits for the swarm to start listen before returning and connecting to peers.
    pub async fn start(&mut self) -> Result<Multiaddr, TransportError<std::io::Error>> {
        match self.swarm.listen_on(self.addr.clone()) {
            Ok(id) => loop {
                if let SwarmEvent::NewListenAddr { address, listener_id } =
                    self.swarm.select_next_some().await
                    && id == listener_id
                {
                    info!(target: "gossip", address = %address, "Listening on address");

                    self.addr = address.clone();

                    return Ok(address);
                }
            },
            Err(err) => {
                error!(target: "gossip", address = %self.addr, error = %err, "Failed to listen on address");
                Err(err)
            }
        }
    }

    /// Returns the local peer id.
    pub fn local_peer_id(&self) -> &libp2p::PeerId {
        self.swarm.local_peer_id()
    }

    /// Returns a mutable reference to the Swarm's behaviour.
    pub fn behaviour_mut(&mut self) -> &mut Behaviour {
        self.swarm.behaviour_mut()
    }

    /// Attempts to select the next event from the swarm, retiring obsolete block
    /// topics periodically even while the network is idle.
    pub async fn next(&mut self) -> Option<SwarmEvent<Event>> {
        loop {
            if TokioInstant::now() >= self.next_topic_retirement {
                let timestamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                self.retire_block_topics(timestamp);
                self.next_topic_retirement = TokioInstant::now() + Self::TOPIC_RETIREMENT_INTERVAL;
            }

            tokio::select! {
                event = self.swarm.next() => return event,
                _ = sleep_until(self.next_topic_retirement) => {}
            }
        }
    }

    /// Unsubscribes obsolete block topics according to a local Unix timestamp.
    ///
    /// Returns the number of subscriptions removed. Future topics are retained
    /// and removed topics are not rejoined if the local clock moves backwards.
    pub fn retire_block_topics(&mut self, timestamp: u64) -> usize {
        let active_topics = self.handler.topics_at(timestamp);
        let mut retired = 0;
        for (version, topic) in [
            ("v1", &self.handler.blocks_v1_topic),
            ("v2", &self.handler.blocks_v2_topic),
            ("v3", &self.handler.blocks_v3_topic),
        ] {
            if !active_topics.contains(&topic.hash())
                && self.swarm.behaviour_mut().gossipsub.unsubscribe(topic)
            {
                info!(target: "gossip", topic = %topic, "Retired block gossip topic");
                Metrics::block_topic_retirements_total(version).increment(1);
                retired += 1;
            }
        }
        #[cfg(feature = "metrics")]
        self.record_block_topic_metrics();
        retired
    }

    /// Samples actual subscriptions and peer membership, including zeroes for retired topics.
    /// Called at startup and on every retirement check, even when the network is idle.
    #[cfg(feature = "metrics")]
    pub fn record_block_topic_metrics(&self) {
        let gossip = &self.swarm.behaviour().gossipsub;
        for (version, topic) in [
            ("v1", &self.handler.blocks_v1_topic),
            ("v2", &self.handler.blocks_v2_topic),
            ("v3", &self.handler.blocks_v3_topic),
            ("v4", &self.handler.blocks_v4_topic),
        ] {
            let hash = topic.hash();
            let subscribed = gossip.topics().any(|topic| *topic == hash);
            Metrics::block_topic_subscribed(version).set(u8::from(subscribed));
            Metrics::block_topic_mesh_peers(version).set(gossip.mesh_peers(&hash).count() as f64);
            let peers = gossip.all_peers().filter(|(_, topics)| topics.contains(&&hash)).count();
            Metrics::block_topic_peers(version).set(peers as f64);
        }
    }

    /// Returns the number of connected peers.
    pub fn connected_peers(&self) -> usize {
        self.swarm.connected_peers().count()
    }

    /// Aborts pending outbound dials that exceeded the connection gate timeout.
    ///
    /// Established peers are removed from pending bookkeeping but are not disconnected.
    pub fn clear_expired_pending_connections(&mut self) -> usize {
        let expired = self.connection_gate.expired_pending_dials();
        let mut cleared = 0;

        for (peer_id, age) in expired {
            if self.swarm.connected_peers().any(|connected| connected == &peer_id) {
                self.connection_gate.remove_dial(&peer_id);
                debug!(
                    target: "gossip",
                    peer_id = %peer_id,
                    pending_dial_age_seconds = age.as_secs_f64(),
                    "Cleared pending dial bookkeeping for connected peer"
                );
                cleared += 1;
                continue;
            }

            let _ = self.swarm.disconnect_peer_id(peer_id);
            self.connection_gate.remove_dial(&peer_id);
            debug!(
                target: "gossip",
                peer_id = %peer_id,
                pending_dial_age_seconds = age.as_secs_f64(),
                "Aborted expired pending outbound dial"
            );
            cleared += 1;
        }

        cleared
    }

    /// Dials the given [`Enr`].
    pub fn dial(&mut self, enr: Enr) {
        let validation = EnrValidation::validate(&enr, self.handler.rollup_config.l2_chain_id.id());
        if validation.is_invalid() {
            trace!(target: "gossip", chain_id = %self.handler.rollup_config.l2_chain_id.id(), validation = %validation, "Invalid Base ENR");
            return;
        }
        let Some(multiaddr) = PeerUtils::enr_to_multiaddr(&enr) else {
            debug!(target: "gossip", enr = ?enr, "Failed to extract tcp socket from enr");
            Metrics::dial_peer_error("invalid_enr").increment(1.0);
            return;
        };
        self.dial_multiaddr(multiaddr);
    }

    /// Dials the given [`Multiaddr`].
    pub fn dial_multiaddr(&mut self, addr: Multiaddr) {
        // Check if we're allowed to dial the address.
        if let Err(connect_error) = self.connection_gate.can_connect_outbound(&addr) {
            debug!(target: "gossip", ?connect_error, "unable to dial peer");
            return;
        }

        // Extract the peer ID from the address.
        let Some(peer_id) = ConnectionGater::peer_id_from_addr(&addr) else {
            warn!(target: "gossip", peer=?addr, "Failed to extract PeerId from Multiaddr");
            return;
        };

        if self.swarm.connected_peers().any(|p| p == &peer_id) {
            debug!(target: "gossip", peer=?addr, "Already connected to peer, not dialing");
            Metrics::dial_peer_error("already_connected").increment(1.0);
            return;
        }

        // Let the gate know we are dialing the address.
        // Note: libp2p-dns will automatically resolve DNS multiaddrs at the transport layer.
        self.connection_gate.dialing(&addr);

        let dial_opts = DialOpts::peer_id(peer_id)
            .addresses(vec![addr.clone()])
            .condition(PeerCondition::DisconnectedAndNotDialing)
            .build();

        match self.swarm.dial(dial_opts) {
            Ok(_) => {
                trace!(target: "gossip", peer=?addr, "Dialed peer");
                self.connection_gate.dialed(&addr);
                Metrics::dial_peer().increment(1.0);
            }
            Err(e) => {
                error!(target: "gossip", error = ?e, "Failed to connect to peer");
                self.connection_gate.remove_dial(&peer_id);
                Metrics::dial_peer_error("connection_error").increment(1.0);
            }
        }
    }

    /// Aborts every outbound connection attempt currently tracked as pending.
    pub fn clear_pending_connections(&mut self) -> usize {
        let pending_dials = self.connection_gate.pending_dials();
        let count = pending_dials.len();

        for peer_id in pending_dials {
            let _ = self.swarm.disconnect_peer_id(peer_id);
            self.connection_gate.remove_dial(&peer_id);
        }

        if count > 0 {
            info!(
                target: "gossip",
                count,
                "Cleared pending outgoing connections"
            );
        }

        count
    }

    fn handle_gossip_event(&mut self, event: Event) -> Option<NetworkPayloadEnvelope> {
        match event {
            Event::Gossipsub(e) => return self.handle_gossipsub_event(*e),
            Event::Ping(libp2p::ping::Event { peer, result, .. }) => {
                trace!(target: "gossip", ?peer, ?result, "Ping received");

                // If the peer is connected to gossip, record the connection duration.
                if let Some(start_time) = self.peer_connection_start.get(&peer) {
                    let _ping_duration = start_time.elapsed();
                    Metrics::gossip_peer_connection_duration_seconds()
                        .record(_ping_duration.as_secs_f64());
                }

                // Record the peer score in the metrics if available.
                if let Some(_peer_score) = self.behaviour_mut().gossipsub.peer_score(&peer) {
                    Metrics::peer_scores().record(_peer_score);
                }

                let pings = Arc::clone(&self.ping);
                tokio::spawn(async move {
                    if let Ok(time) = result {
                        pings.lock().await.insert(peer, time);
                    }
                });
            }
            Event::Identify(e) => self.handle_identify_event(*e),
        };

        None
    }

    fn handle_identify_event(&mut self, event: libp2p::identify::Event) {
        match event {
            libp2p::identify::Event::Received { connection_id, peer_id, info } => {
                debug!(target: "gossip", ?connection_id, peer_id = %peer_id, ?info, "Received identify info from peer");
                self.prune_peerstore_for_new_peer(peer_id);
                self.peerstore.put(peer_id, info);
            }
            libp2p::identify::Event::Sent { connection_id, peer_id } => {
                debug!(target: "gossip", ?connection_id, peer_id = %peer_id, "Sent identify info to peer");
            }
            libp2p::identify::Event::Pushed { connection_id, peer_id, info } => {
                debug!(target: "gossip", ?connection_id, peer_id = %peer_id, ?info, "Pushed identify info to peer");
            }
            libp2p::identify::Event::Error { connection_id, peer_id, error } => {
                error!(target: "gossip", ?connection_id, peer_id = %peer_id, ?error, "Error raised while attempting to identify remote");
            }
        }
    }

    fn prune_peerstore_for_new_peer(&mut self, peer_id: PeerId) {
        if self.peerstore.contains(&peer_id) || self.peerstore.len() < self.peerstore.cap().get() {
            return;
        }

        let connected_peers = self.swarm.connected_peers().copied().collect::<HashSet<_>>();
        let peer_to_remove = peerstore_eviction_candidate(&self.peerstore, &connected_peers)
            .expect("peerstore is non-empty when at capacity");

        self.peerstore.pop(&peer_to_remove);

        // This is a cache-level cap, not Lighthouse's full peer lifecycle model. Lighthouse
        // keeps explicit connection state and evicts excess disconnected, untrusted peers; Base
        // currently only has identify metadata here, so prefer disconnected entries and bound
        // the cache until we model peer lifecycle state directly.
        debug!(
            target: "gossip",
            peer_id = %peer_to_remove,
            peerstore_size = self.peerstore.len(),
            peerstore_limit = self.peerstore.cap().get(),
            "Evicted identify info from peerstore"
        );
    }

    /// Handles a [`libp2p::gossipsub::Event`].
    fn handle_gossipsub_event(
        &mut self,
        event: libp2p::gossipsub::Event,
    ) -> Option<NetworkPayloadEnvelope> {
        match event {
            libp2p::gossipsub::Event::Message {
                propagation_source: src,
                message_id: id,
                message,
            } => {
                trace!(target: "gossip", topic = %message.topic, "Received message");
                Metrics::gossip_event("message").increment(1.0);
                // Subscription state makes retirement irreversible; the handler
                // checks the clock once, before decoding, between sweeps.
                if self.swarm.behaviour().gossipsub.topics().any(|topic| *topic == message.topic) {
                    let (status, payload) = self.handler.handle(message);
                    _ = self
                        .swarm
                        .behaviour_mut()
                        .gossipsub
                        .report_message_validation_result(&id, &src, status);
                    return payload;
                }
                if let Some(version) = self.handler.topic_version(&message.topic) {
                    Metrics::block_topic_blocked_total(version, "inbound").increment(1);
                }
            }
            libp2p::gossipsub::Event::Subscribed { peer_id, topic } => {
                trace!(target: "gossip", peer_id = %peer_id, topic = ?topic, "Peer subscribed");
                Metrics::gossip_event("subscribed").increment(1.0);
            }
            libp2p::gossipsub::Event::Unsubscribed { peer_id, topic } => {
                trace!(target: "gossip", peer_id = %peer_id, topic = ?topic, "Peer unsubscribed");
                Metrics::gossip_event("unsubscribed").increment(1.0);
            }
            libp2p::gossipsub::Event::SlowPeer { peer_id, .. } => {
                trace!(target: "gossip", peer_id = %peer_id, "Slow peer");
                Metrics::gossip_event("slow_peer").increment(1.0);
            }
            libp2p::gossipsub::Event::GossipsubNotSupported { peer_id } => {
                trace!(target: "gossip", peer_id = %peer_id, "Peer does not support gossipsub");
                Metrics::gossip_event("not_supported").increment(1.0);
            }
        }
        None
    }

    /// Handles the [`SwarmEvent<Event>`].
    pub fn handle_event(&mut self, event: SwarmEvent<Event>) -> Option<NetworkPayloadEnvelope> {
        match event {
            SwarmEvent::Behaviour(behavior_event) => {
                return self.handle_gossip_event(behavior_event);
            }
            SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } => {
                self.connection_gate.remove_dial(&peer_id);

                if endpoint.is_listener() {
                    let addr = endpoint.get_remote_address();
                    if let Err(error) = self.connection_gate.can_connect_inbound(&peer_id, addr) {
                        debug!(target: "gossip", peer_id = %peer_id, addr = %addr, error = ?error, "Closing blocked inbound connection");
                        self.swarm.close_connection(connection_id);
                        Metrics::gossipsub_connection("blocked_inbound").increment(1.0);
                        return None;
                    }
                }

                let peer_count = self.swarm.connected_peers().count();
                debug!(target: "gossip", peer_id = %peer_id, peer_count, "Connection established");
                Metrics::gossipsub_connection("connected").increment(1.0);
                Metrics::gossip_peer_count().set(peer_count as f64);

                self.peer_connection_start.insert(peer_id, Instant::now());
            }
            SwarmEvent::OutgoingConnectionError { peer_id: _peer_id, error, .. } => {
                debug!(target: "gossip", error = ?error, "Outgoing connection error");
                // Remove the peer from current_dials so it can be dialed again
                if let Some(peer_id) = _peer_id {
                    self.connection_gate.remove_dial(&peer_id);
                }
                Metrics::gossipsub_connection("outgoing_error").increment(1.0);
            }
            SwarmEvent::IncomingConnectionError {
                error, connection_id: _connection_id, ..
            } => {
                debug!(target: "gossip", error = ?error, "Incoming connection error");
                Metrics::gossipsub_connection("incoming_error").increment(1.0);
            }
            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                let peer_count = self.swarm.connected_peers().count();
                debug!(target: "gossip", ?peer_id, ?cause, peer_count, "Connection closed");
                Metrics::gossipsub_connection("closed").increment(1.0);
                Metrics::gossip_peer_count().set(peer_count as f64);

                // Record the total connection duration.
                if let Some(start_time) = self.peer_connection_start.remove(&peer_id) {
                    Metrics::gossip_peer_connection_duration_seconds()
                        .record(start_time.elapsed().as_secs_f64());
                }

                // Record the peer score in the metrics if available.
                if let Some(_peer_score) = self.behaviour_mut().gossipsub.peer_score(&peer_id) {
                    Metrics::peer_scores().record(_peer_score);
                }

                let pings = Arc::clone(&self.ping);
                tokio::spawn(async move {
                    pings.lock().await.remove(&peer_id);
                });

                // If the connection was initiated by us, remove the peer from the current dials
                // set so that we can dial it again.
                self.connection_gate.remove_dial(&peer_id);
            }
            SwarmEvent::NewListenAddr { listener_id, address } => {
                debug!(target: "gossip", reporter_id = ?listener_id, new_address = ?address, "New listen address");
            }
            SwarmEvent::Dialing { peer_id, connection_id } => {
                debug!(target: "gossip", ?peer_id, ?connection_id, "Dialing peer");
            }
            SwarmEvent::NewExternalAddrOfPeer { peer_id, address } => {
                debug!(target: "gossip", ?peer_id, ?address, "New external address of peer");
            }
            _ => {
                debug!(target: "gossip", ?event, "Ignoring non-behaviour in event handler");
            }
        };

        None
    }
}

fn peerstore_eviction_candidate<T>(
    peerstore: &LruCache<PeerId, T>,
    connected_peers: &HashSet<PeerId>,
) -> Option<PeerId> {
    peerstore
        .iter()
        .rev()
        .map(|(peer_id, _)| *peer_id)
        .find(|peer_id| !connected_peers.contains(peer_id))
        .or_else(|| peerstore.iter().next_back().map(|(peer_id, _)| *peer_id))
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain;
    use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
    use alloy_primitives::{B256, Signature};
    use alloy_rpc_types_engine::{ExecutionPayloadV2, ExecutionPayloadV3};
    use base_common_genesis::UpgradeConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadV4, PayloadHash};
    use libp2p::gossipsub::{Message, MessageAcceptance};
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// A real TCP gossip pair. The second peer retains its original subscriptions
    /// to model a node that has not implemented topic retirement yet.
    #[derive(std::fmt::Debug)]
    pub struct GossipPair {
        /// Peer running the production retirement loop.
        pub retiring: GossipDriver<ConnectionGater>,
        /// Peer retaining the subscriptions established at startup.
        pub legacy: GossipDriver<ConnectionGater>,
    }

    impl GossipPair {
        /// Drives both real swarms and fails if their connection closes.
        pub async fn poll(&mut self) -> Option<NetworkPayloadEnvelope> {
            tokio::select! {
                event = self.retiring.next() => {
                    let event = event.expect("retiring swarm remains open");
                    assert!(!matches!(event, SwarmEvent::ConnectionClosed { .. }), "{event:?}");
                    self.retiring.handle_event(event)
                }
                // Poll the swarm directly so this peer keeps advertising old topics.
                event = self.legacy.swarm.next() => {
                    let event = event.expect("legacy swarm remains open");
                    assert!(!matches!(event, SwarmEvent::ConnectionClosed { .. }), "{event:?}");
                    self.legacy.handle_event(event)
                }
            }
        }

        /// Publishes a fresh V4 payload and waits for its validated delivery.
        pub async fn publish_v4(&mut self) {
            let mut block = crate::v4_valid_block();
            block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
            let payload = BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                ExecutionPayloadV3::from_block_slow(&block),
                block.header.withdrawals_root.unwrap(),
            );
            let envelope = NetworkPayloadEnvelope {
                payload: BaseExecutionPayload::V4(payload),
                signature: Signature::test_signature(),
                payload_hash: PayloadHash(B256::ZERO),
                parent_beacon_block_root: block.header.parent_beacon_block_root,
            };
            let decoded =
                NetworkPayloadEnvelope::decode_v4(&envelope.encode_v4().unwrap()).unwrap();
            let signing_hash = decoded
                .payload_hash
                .signature_message(self.retiring.handler.rollup_config.l2_chain_id.id());
            let signer = decoded.signature.recover_address_from_prehash(&signing_hash).unwrap();
            let (_sender, receiver) = tokio::sync::watch::channel(signer);
            self.retiring.handler.signer_recv = receiver;
            let expected_hash = envelope.payload.block_hash();
            self.legacy.publish(|handler| handler.blocks_v4_topic.clone(), Some(envelope)).unwrap();
            loop {
                if let Some(received) = self.poll().await {
                    assert_eq!(received.payload.block_hash(), expected_hash);
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn tcp_peer_observes_retirement_without_losing_v4_propagation() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut pair = GossipPair { retiring: test_driver(), legacy: test_driver() };
            let address = pair.retiring.start().await.unwrap();
            pair.legacy.start().await.unwrap();
            pair.legacy.swarm.dial(address).unwrap();
            let v3 = pair.retiring.handler.blocks_v3_topic.hash();
            let v4 = pair.retiring.handler.blocks_v4_topic.hash();

            // Both old and future topic meshes must actually form before retirement.
            while [&pair.retiring, &pair.legacy].iter().any(|driver| {
                let gossip = &driver.swarm.behaviour().gossipsub;
                gossip.mesh_peers(&v3).count() != 1 || gossip.mesh_peers(&v4).count() != 1
            }) {
                pair.poll().await;
            }

            // Activate Isthmus within its grace period, without a wall-clock sleep.
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
            pair.retiring.handler.rollup_config.upgrades.isthmus_time = Some(now);
            pair.legacy.handler.rollup_config.upgrades.isthmus_time = Some(now);
            assert_eq!(pair.retiring.retire_block_topics(now + 59), 0);
            pair.publish_v4().await;

            // Advance the retirement action to the exact boundary. The legacy
            // peer must learn the unsubscribe over the wire, not by shared state.
            assert_eq!(pair.retiring.retire_block_topics(now + 60), 3);
            loop {
                if pair
                    .legacy
                    .swarm
                    .behaviour()
                    .gossipsub
                    .all_peers()
                    .any(|(peer, topics)| peer == pair.retiring.local_peer_id() && topics == [&v4])
                {
                    break;
                }
                pair.poll().await;
            }
            assert_eq!(pair.retiring.retire_block_topics(now), 0, "rollback must not rejoin");
            assert!(pair.retiring.swarm.is_connected(pair.legacy.local_peer_id()));
            assert_eq!(pair.retiring.swarm.behaviour().gossipsub.mesh_peers(&v4).count(), 1);
            assert_eq!(pair.legacy.swarm.behaviour().gossipsub.mesh_peers(&v4).count(), 1);
            pair.publish_v4().await;

            #[cfg(feature = "metrics")]
            {
                let recorder = PrometheusBuilder::new().build_recorder();
                metrics::with_local_recorder(&recorder, || {
                    pair.retiring.record_block_topic_metrics();
                });
                let output = recorder.handle().render();
                // A legacy peer still advertises V3, but is no longer in our V3 mesh.
                assert!(output.contains("base_node_block_topic_subscribed{version=\"v3\"} 0"));
                assert!(output.contains("base_node_block_topic_peers{version=\"v3\"} 1"));
                assert!(output.contains("base_node_block_topic_mesh_peers{version=\"v3\"} 0"));
                assert!(output.contains("base_node_block_topic_subscribed{version=\"v4\"} 1"));
                assert!(output.contains("base_node_block_topic_peers{version=\"v4\"} 1"));
                assert!(output.contains("base_node_block_topic_mesh_peers{version=\"v4\"} 1"));
            }
        })
        .await
        .expect("mesh formation, unsubscribe and V4 delivery must complete");
    }

    #[test]
    #[cfg(feature = "metrics")]
    fn topic_metrics_report_startup_retirement_and_rollback() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut driver = test_driver();
            for version in ["v1", "v2", "v3", "v4"] {
                assert!(handle.render().contains(&format!(
                    "base_node_block_topic_subscribed{{version=\"{version}\"}} 1"
                )));
            }
            driver.handler.rollup_config.upgrades.isthmus_time = Some(100);
            driver.retire_block_topics(160);
            driver.retire_block_topics(160);
            driver.retire_block_topics(0);
            let output = handle.render();
            for version in ["v1", "v2", "v3"] {
                assert!(output.contains(&format!(
                    "base_node_block_topic_subscribed{{version=\"{version}\"}} 0"
                )));
                assert!(output.contains(&format!(
                    "base_node_block_topic_retirements_total{{version=\"{version}\"}} 1"
                )));
            }
            assert!(output.contains("base_node_block_topic_subscribed{version=\"v4\"} 1"));
            assert!(output.contains("base_node_block_topic_mesh_peers{version=\"v3\"} 0"));
            assert!(output.contains("base_node_block_topic_peers{version=\"v4\"} 0"));
        });
    }

    #[test]
    #[cfg(feature = "metrics")]
    fn startup_metrics_distinguish_skipped_topics_from_runtime_retirement() {
        let recorder = PrometheusBuilder::new().build_recorder();
        metrics::with_local_recorder(&recorder, || {
            let config = RollupConfig {
                upgrades: UpgradeConfig { isthmus_time: Some(0), ..Default::default() },
                ..Default::default()
            };
            let (_driver, _signer) = GossipDriver::<ConnectionGater>::builder(
                config,
                Address::ZERO,
                "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
                Keypair::generate_secp256k1(),
            )
            .build()
            .unwrap();
        });
        let output = recorder.handle().render();
        for version in ["v1", "v2", "v3"] {
            assert!(
                output.contains(&format!(
                    "base_node_block_topic_subscribed{{version=\"{version}\"}} 0"
                ))
            );
        }
        assert!(output.contains("base_node_block_topic_subscribed{version=\"v4\"} 1"));
        assert!(!output.contains("base_node_block_topic_retirements_total"));
    }

    fn test_driver() -> GossipDriver<ConnectionGater> {
        let rollup_config = RollupConfig {
            l2_chain_id: Chain::base_mainnet(),
            block_time: 2,
            ..Default::default()
        };

        let (driver, _signer) = GossipDriver::<ConnectionGater>::builder(
            rollup_config,
            Address::repeat_byte(0x11),
            "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            Keypair::generate_secp256k1(),
        )
        .build()
        .unwrap();

        driver
    }

    #[test]
    fn obsolete_topics_leave_the_mesh_after_the_grace_period() {
        let mut driver = test_driver();
        driver.handler.rollup_config.upgrades = UpgradeConfig {
            canyon_time: Some(100),
            ecotone_time: Some(200),
            isthmus_time: Some(300),
            ..Default::default()
        };
        let all = [
            driver.handler.blocks_v1_topic.hash(),
            driver.handler.blocks_v2_topic.hash(),
            driver.handler.blocks_v3_topic.hash(),
            driver.handler.blocks_v4_topic.hash(),
        ];
        for (timestamp, first_topic, removed) in [
            (159, 0, 0),
            (160, 1, 1),
            (160, 1, 0),
            (259, 1, 0),
            (260, 2, 1),
            (359, 2, 0),
            (360, 3, 1),
            (0, 3, 0),
        ] {
            assert_eq!(driver.retire_block_topics(timestamp), removed);
            let mut subscribed =
                driver.swarm.behaviour().gossipsub.topics().cloned().collect::<Vec<_>>();
            subscribed.sort();
            assert_eq!(subscribed, all[first_topic..], "timestamp {timestamp}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_polling_retires_topics_without_an_inbound_message() {
        let mut driver = test_driver();
        // Complete the initial no-op retirement check, then cancel the idle
        // poll. Subsequent checks must still run without a swarm event.
        assert!(tokio::time::timeout(Duration::from_millis(1), driver.next()).await.is_err());
        assert_eq!(driver.swarm.behaviour().gossipsub.topics().count(), 4);

        // Make retirement due after startup without depending on wall-clock
        // sleeps. Tokio's paused clock advances the periodic poll deadline.
        driver.handler.rollup_config.upgrades.isthmus_time = Some(0);
        assert!(
            tokio::time::timeout(
                GossipDriver::<ConnectionGater>::TOPIC_RETIREMENT_INTERVAL * 2,
                driver.next()
            )
            .await
            .is_err()
        );
        assert_eq!(
            driver.swarm.behaviour().gossipsub.topics().cloned().collect::<Vec<_>>(),
            [driver.handler.blocks_v4_topic.hash()]
        );
    }

    #[test]
    fn retired_topics_are_blocked_before_the_subscription_sweep() {
        #[cfg(feature = "metrics")]
        let recorder = PrometheusBuilder::new().build_recorder();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut driver = test_driver();
        // Make the clock cutoff due while the startup subscription still exists.
        driver.handler.rollup_config.upgrades.isthmus_time = Some(0);
        let topic = driver.handler.blocks_v2_topic.hash();
        assert!(driver.swarm.behaviour().gossipsub.topics().any(|subscribed| *subscribed == topic));
        let envelope = NetworkPayloadEnvelope {
            payload: BaseExecutionPayload::V2(ExecutionPayloadV2::from_block_slow(
                &crate::v2_valid_block(),
            )),
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };
        assert!(matches!(
            driver.publish(|handler| handler.blocks_v2_topic.clone(), Some(envelope)),
            Err(PublishError::EncodeError(HandlerEncodeError::UnknownTopic(_)))
        ));
        let event =
            SwarmEvent::Behaviour(Event::Gossipsub(Box::new(libp2p::gossipsub::Event::Message {
                propagation_source: PeerId::random(),
                message_id: MessageId(vec![1]),
                message: Message { source: None, sequence_number: None, topic, data: vec![0xff] },
            })));
        assert!(driver.handle_event(event).is_none());
        #[cfg(feature = "metrics")]
        {
            let output = recorder.handle().render();
            for direction in ["inbound", "outbound"] {
                assert!(output.contains(&format!(
                    "base_node_block_topic_blocked_total{{version=\"v2\",direction=\"{direction}\"}} 1"
                )), "{output}");
            }
        }
    }

    #[test]
    fn retired_topics_stay_disabled_after_a_clock_rollback() {
        #[cfg(feature = "metrics")]
        let recorder = PrometheusBuilder::new().build_recorder();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut driver = test_driver();
        driver.handler.rollup_config.upgrades.isthmus_time = Some(100);
        assert_eq!(driver.retire_block_topics(160), 3);
        // Even if the clock/config subsequently permits an older version,
        // GossipSub's ability to publish without subscribing must not revive it.
        driver.handler.rollup_config.upgrades = UpgradeConfig::default();
        let envelope = NetworkPayloadEnvelope {
            payload: BaseExecutionPayload::V2(ExecutionPayloadV2::from_block_slow(
                &crate::v2_valid_block(),
            )),
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };
        let encoded = envelope.encode_v2().unwrap();
        let decoded = NetworkPayloadEnvelope::decode_v2(&encoded).unwrap();
        let signing_hash =
            decoded.payload_hash.signature_message(driver.handler.rollup_config.l2_chain_id.id());
        let signer = decoded.signature.recover_address_from_prehash(&signing_hash).unwrap();
        let (_, receiver) = tokio::sync::watch::channel(signer);
        driver.handler.signer_recv = receiver;
        let message = Message {
            source: None,
            sequence_number: None,
            topic: driver.handler.blocks_v2_topic.hash(),
            data: encoded,
        };
        // The restored policy and signature would otherwise accept this block.
        assert!(matches!(
            driver.handler.clone().handle(message.clone()),
            (MessageAcceptance::Accept, Some(_))
        ));

        assert!(matches!(
            driver.publish(|handler| handler.blocks_v2_topic.clone(), Some(envelope)),
            Err(PublishError::EncodeError(HandlerEncodeError::UnknownTopic(_)))
        ));
        let event =
            SwarmEvent::Behaviour(Event::Gossipsub(Box::new(libp2p::gossipsub::Event::Message {
                propagation_source: PeerId::random(),
                message_id: MessageId(vec![1]),
                message,
            })));
        assert!(driver.handle_event(event).is_none());
        #[cfg(feature = "metrics")]
        {
            let output = recorder.handle().render();
            for direction in ["inbound", "outbound"] {
                assert!(output.contains(&format!(
                    "base_node_block_topic_blocked_total{{version=\"v2\",direction=\"{direction}\"}} 1"
                )), "{output}");
            }
        }
    }

    #[tokio::test]
    async fn clear_pending_connections_aborts_known_peer_dials() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            if let Ok((_socket, _addr)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let peer_id = PeerId::random();
        let addr = format!("/ip4/127.0.0.1/tcp/{port}/p2p/{peer_id}").parse().unwrap();
        let mut driver = test_driver();

        driver.dial_multiaddr(addr);
        assert!(driver.connection_gate.current_dials.contains_key(&peer_id));

        assert_eq!(driver.clear_pending_connections(), 1);
        assert!(!driver.connection_gate.current_dials.contains_key(&peer_id));

        let event = tokio::time::timeout(Duration::from_secs(1), driver.next())
            .await
            .expect("clearing a known-peer dial should emit an event")
            .expect("swarm should emit the aborted dial event");

        match event {
            SwarmEvent::OutgoingConnectionError {
                peer_id: Some(aborted_peer),
                error: libp2p::swarm::DialError::Aborted,
                ..
            } => assert_eq!(aborted_peer, peer_id),
            event => panic!("expected aborted outgoing connection event, got {event:?}"),
        }

        accept_task.abort();
    }

    #[test]
    fn clear_pending_connections_returns_zero_when_empty() {
        let mut driver = test_driver();

        assert_eq!(driver.clear_pending_connections(), 0);
    }

    #[tokio::test]
    async fn established_connections_are_removed_from_pending_dials() {
        let mut dialer = test_driver();
        let mut listener = test_driver();
        let listener_peer_id = *listener.local_peer_id();
        let mut listener_addr = listener.start().await.unwrap();
        listener_addr.push(libp2p::multiaddr::Protocol::P2p(listener_peer_id));

        dialer.dial_multiaddr(listener_addr);
        assert!(dialer.connection_gate.current_dials.contains_key(&listener_peer_id));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline
            && dialer.connection_gate.current_dials.contains_key(&listener_peer_id)
        {
            tokio::select! {
                event = dialer.next() => {
                    if let Some(event) = event {
                        dialer.handle_event(event);
                    }
                }
                event = listener.next() => {
                    if let Some(event) = event {
                        listener.handle_event(event);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }

        assert!(!dialer.connection_gate.current_dials.contains_key(&listener_peer_id));
    }

    #[test]
    fn test_peerstore_eviction_candidate_prefers_disconnected_peer() {
        let connected_peer = PeerId::random();
        let disconnected_peer = PeerId::random();
        let mut peerstore = LruCache::new(NonZeroUsize::new(1024).unwrap());
        peerstore.put(disconnected_peer, ());
        peerstore.put(connected_peer, ());
        let connected_peers = HashSet::from([connected_peer]);

        let candidate = peerstore_eviction_candidate(&peerstore, &connected_peers);

        assert_eq!(candidate, Some(disconnected_peer));
    }

    #[test]
    fn test_peerstore_eviction_candidate_uses_lru_disconnected_peer() {
        let oldest_disconnected_peer = PeerId::random();
        let newest_disconnected_peer = PeerId::random();
        let connected_peer = PeerId::random();
        let mut peerstore = LruCache::new(NonZeroUsize::new(1024).unwrap());
        peerstore.put(oldest_disconnected_peer, ());
        peerstore.put(newest_disconnected_peer, ());
        peerstore.put(connected_peer, ());
        let connected_peers = HashSet::from([connected_peer]);

        let candidate = peerstore_eviction_candidate(&peerstore, &connected_peers);

        assert_eq!(candidate, Some(oldest_disconnected_peer));
    }

    #[test]
    fn test_peerstore_eviction_candidate_falls_back_to_lru_connected_peer() {
        let least_recent_peer = PeerId::random();
        let most_recent_peer = PeerId::random();
        let mut peerstore = LruCache::new(NonZeroUsize::new(1024).unwrap());
        peerstore.put(least_recent_peer, ());
        peerstore.put(most_recent_peer, ());
        let connected_peers = HashSet::from([least_recent_peer, most_recent_peer]);

        let candidate = peerstore_eviction_candidate(&peerstore, &connected_peers);

        assert_eq!(candidate, Some(least_recent_peer));
    }
}
