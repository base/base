//! Consensus-layer gossipsub driver for Base.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
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
use tokio::sync::Mutex;

use crate::{
    Behaviour, BlockHandler, ConnectionGate, ConnectionGater, ConnectionLimitsConfig, Event,
    GossipDriverBuilder, Handler, Metrics, PublishError,
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
}

impl<G> GossipDriver<G>
where
    G: ConnectionGate,
{
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
        Self {
            swarm,
            addr,
            handler,
            peerstore: LruCache::new(config.max_identify_peerstore_peers),
            peer_monitoring: config.peer_monitoring,
            peer_connection_start: Default::default(),
            connection_gate: gate,
            connection_limits_config: config.connection_limits_config,
            ping: Arc::new(Mutex::new(Default::default())),
        }
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

    /// Attempts to select the next event from the Swarm.
    pub async fn next(&mut self) -> Option<SwarmEvent<Event>> {
        self.swarm.next().await
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
                if self.handler.topics().contains(&message.topic) {
                    let (status, payload) = self.handler.handle(message);
                    _ = self
                        .swarm
                        .behaviour_mut()
                        .gossipsub
                        .report_message_validation_result(&id, &src, status);
                    return payload;
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
    use alloy_rpc_types_engine::ExecutionPayloadV3;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadV4, PayloadHash};

    use super::*;

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

    #[tokio::test]
    async fn v4_gossip_works_without_legacy_sync_advertisement() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut sender = test_driver();
            let mut receiver = test_driver();
            sender.handler.rollup_config.upgrades.isthmus_time = Some(0);
            receiver.handler.rollup_config.upgrades.isthmus_time = Some(0);
            let address = receiver.start().await.unwrap();
            sender.start().await.unwrap();
            sender.swarm.dial(address).unwrap();
            let topic = receiver.handler.blocks_v4_topic.hash();

            // Wait for actual Identify exchange and the current block-topic mesh.
            while sender.peerstore.peek(receiver.local_peer_id()).is_none()
                || receiver.peerstore.peek(sender.local_peer_id()).is_none()
                || sender.swarm.behaviour().gossipsub.mesh_peers(&topic).count() == 0
                || receiver.swarm.behaviour().gossipsub.mesh_peers(&topic).count() == 0
            {
                tokio::select! {
                    event = sender.next() => sender.handle_event(event.unwrap()),
                    event = receiver.next() => receiver.handle_event(event.unwrap()),
                };
            }
            for info in [
                sender.peerstore.peek(receiver.local_peer_id()).unwrap(),
                receiver.peerstore.peek(sender.local_peer_id()).unwrap(),
            ] {
                assert!(info.protocols.iter().any(|protocol| protocol.as_ref().starts_with("/meshsub/")));
                assert!(info.protocols.iter().all(|protocol| !protocol.as_ref().starts_with("/opstack/req/payload_by_number/")));
            }

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
            let decoded = NetworkPayloadEnvelope::decode_v4(&envelope.encode_v4().unwrap()).unwrap();
            let signing_hash = decoded.payload_hash.signature_message(receiver.handler.rollup_config.l2_chain_id.id());
            let signer = decoded.signature.recover_address_from_prehash(&signing_hash).unwrap();
            let (_signer_tx, signer_rx) = tokio::sync::watch::channel(signer);
            receiver.handler.signer_recv = signer_rx;
            sender.publish(|handler| handler.blocks_v4_topic.clone(), Some(envelope.clone())).unwrap();

            loop {
                tokio::select! {
                    event = sender.next() => { sender.handle_event(event.unwrap()); }
                    event = receiver.next() => {
                        if let Some(received) = receiver.handle_event(event.unwrap()) {
                            assert_eq!(received.payload, envelope.payload);
                            assert_eq!(received.parent_beacon_block_root, envelope.parent_beacon_block_root);
                            break;
                        }
                    }
                }
            }
            assert!(sender.swarm.is_connected(receiver.local_peer_id()));
            assert!(receiver.swarm.is_connected(sender.local_peer_id()));
        }).await.expect("Identify exchange and V4 propagation must complete without legacy sync");
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
