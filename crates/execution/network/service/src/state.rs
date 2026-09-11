//! Keeps track of the state of the network.

use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize},
    },
    task::{Context, Poll},
};

use alloy_eip2124::ForkId;
use alloy_primitives::map::{FbBuildHasher, HashMap};
use base_execution_network_wire::{
    Capabilities, DisconnectReason, GetReceipts70, PeerAddr, PeerId, PeerKind, ReceiptsResponse,
};
use tokio::sync::oneshot;
use tracing::{debug, trace};

use crate::{
    DiscoveredEvent, DiscoveryEvent, FetchClient, PeerRequest, PeerRequestSender,
    discovery::Discovery,
    fetch::{BlockResponseOutcome, FetchAction, NewPeerInfo, StateFetcher},
    message::{BlockRequest, PeerResponse, PeerResponseResult},
    peers::{PeerAction, PeersManager},
    session::BlockRangeInfo,
};

/// The [`NetworkState`] keeps track of the state of all peers in the network.
///
/// This includes:
///   - [`Discovery`]: manages the discovery protocol, essentially a stream of discovery updates
///   - [`PeersManager`]: keeps track of connected peers and issues new outgoing connections
///     depending on the configured capacity.
///   - [`StateFetcher`]: streams download request (received from outside via channel) which are
///     then send to the session of the peer.
///
/// This type is also responsible for responding for received request.
#[derive(Debug)]
pub struct NetworkState {
    /// All active peers and their state.
    active_peers: HashMap<PeerId, ActivePeer, FbBuildHasher<64>>,
    /// Manages connections to peers.
    peers_manager: PeersManager,
    /// Buffered messages until polled.
    queued_messages: VecDeque<StateAction>,
    /// Network discovery.
    discovery: Discovery,
    /// The type that handles requests.
    ///
    /// The fetcher streams `RLPx` related requests on a per-peer basis to this type. This type
    /// will then queue in the request and notify the fetcher once the result has been
    /// received.
    state_fetcher: StateFetcher,
}

impl NetworkState {
    /// Create a new state instance with the given params
    pub(crate) fn new(
        discovery: Discovery,
        peers_manager: PeersManager,
        num_active_peers: Arc<AtomicUsize>,
    ) -> Self {
        let state_fetcher = StateFetcher::new(peers_manager.handle(), num_active_peers);
        Self {
            active_peers: Default::default(),
            peers_manager,
            queued_messages: Default::default(),
            discovery,
            state_fetcher,
        }
    }

    /// Returns mutable access to the [`PeersManager`]
    pub(crate) const fn peers_mut(&mut self) -> &mut PeersManager {
        &mut self.peers_manager
    }

    /// Returns mutable access to the [`Discovery`]
    pub(crate) const fn discovery_mut(&mut self) -> &mut Discovery {
        &mut self.discovery
    }

    /// Returns access to the [`PeersManager`]
    pub(crate) const fn peers(&self) -> &PeersManager {
        &self.peers_manager
    }

    /// Returns a new [`FetchClient`]
    pub(crate) fn fetch_client(&self) -> FetchClient {
        self.state_fetcher.client()
    }

    /// How many peers we're currently connected to.
    pub fn num_active_peers(&self) -> usize {
        self.active_peers.len()
    }

    /// Event hook for an activated session for the peer.
    ///
    /// Returns `Ok` if the session is valid, returns an `Err` if the session is not accepted and
    /// should be rejected.
    pub(crate) fn on_session_activated(&mut self, activation: SessionActivation) {
        let SessionActivation {
            peer,
            capabilities,
            request_tx,
            timeout,
            range_info,
            supports_snap,
        } = activation;

        debug_assert!(!self.active_peers.contains_key(&peer), "Already connected; not possible");

        self.state_fetcher.new_active_peer(NewPeerInfo {
            peer_id: peer,
            capabilities: Arc::clone(&capabilities),
            timeout,
            range_info,
            supports_snap,
        });

        self.active_peers
            .insert(peer, ActivePeer { capabilities, request_tx, pending_response: None });
    }

    /// Event hook for a disconnected session for the given peer.
    ///
    /// This will remove the peer from the available set of peers and close all inflight requests.
    pub(crate) fn on_session_closed(&mut self, peer: PeerId) {
        self.active_peers.remove(&peer);
        self.state_fetcher.on_session_closed(&peer);
    }

    /// Invoked when a new [`ForkId`] is activated.
    pub(crate) fn update_fork_id(&self, fork_id: ForkId) {
        self.discovery.update_fork_id(fork_id)
    }

    /// Bans the [`IpAddr`] in the discovery service.
    pub(crate) fn ban_ip_discovery(&self, ip: IpAddr) {
        trace!(target: "net", ?ip, "Banning discovery");
        self.discovery.ban_ip(ip)
    }

    /// Bans the [`PeerId`] and [`IpAddr`] in the discovery service.
    pub(crate) fn ban_discovery(&self, peer_id: PeerId, ip: IpAddr) {
        trace!(target: "net", ?peer_id, ?ip, "Banning discovery");
        self.discovery.ban(peer_id, ip)
    }

    /// Marks the given peer as trusted.
    pub(crate) fn add_trusted_peer_id(&mut self, peer_id: PeerId) {
        self.peers_manager.add_trusted_peer_id(peer_id)
    }

    /// Adds a trusted peer that may use a hostname, with periodic DNS re-resolution.
    pub(crate) fn add_trusted_peer_node(
        &mut self,
        trusted: base_execution_network_wire::TrustedPeer,
    ) {
        self.peers_manager.add_trusted_peer_node(trusted)
    }

    /// Adds a peer and its address with the given kind to the peerset.
    pub(crate) fn add_peer_kind(
        &mut self,
        peer_id: PeerId,
        kind: Option<PeerKind>,
        addr: PeerAddr,
    ) {
        self.peers_manager.add_peer_kind(peer_id, kind, addr, None)
    }

    /// Connects a peer and its address with the given kind
    pub(crate) fn add_and_connect(&mut self, peer_id: PeerId, kind: PeerKind, addr: PeerAddr) {
        self.peers_manager.add_and_connect_kind(peer_id, kind, addr, None)
    }

    /// Removes a peer and its address with the given kind from the peerset.
    pub(crate) fn remove_peer_kind(&mut self, peer_id: PeerId, kind: PeerKind) {
        match kind {
            PeerKind::Basic | PeerKind::Static => self.peers_manager.remove_peer(peer_id),
            PeerKind::Trusted => self.peers_manager.remove_peer_from_trusted_set(peer_id),
        }
    }

    /// Event hook for events received from the discovery service.
    fn on_discovery_event(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::NewNode(DiscoveredEvent::EventQueued { peer_id, addr, fork_id }) => {
                self.queued_messages.push_back(StateAction::DiscoveredNode {
                    peer_id,
                    addr,
                    fork_id,
                });
            }
            DiscoveryEvent::EnrForkId(record, fork_id) => {
                let peer_id = record.id;
                let tcp_addr = record.tcp_addr();
                if tcp_addr.port() == 0 {
                    return;
                }
                let udp_addr = record.udp_addr();
                let addr = PeerAddr::new(tcp_addr, Some(udp_addr));
                self.queued_messages.push_back(StateAction::DiscoveredEnrForkId {
                    peer_id,
                    addr,
                    fork_id,
                });
            }
        }
    }

    /// Event hook for new actions derived from the peer management set.
    fn on_peer_action(&mut self, action: PeerAction) {
        match action {
            PeerAction::Connect { peer_id, remote_addr } => {
                self.queued_messages.push_back(StateAction::Connect { peer_id, remote_addr });
            }
            PeerAction::Disconnect { peer_id, reason } => {
                self.state_fetcher.on_pending_disconnect(&peer_id);
                self.queued_messages.push_back(StateAction::Disconnect { peer_id, reason });
            }
            PeerAction::DisconnectBannedIncoming { peer_id }
            | PeerAction::DisconnectUntrustedIncoming { peer_id } => {
                self.state_fetcher.on_pending_disconnect(&peer_id);
                self.queued_messages.push_back(StateAction::Disconnect { peer_id, reason: None });
            }
            PeerAction::DiscoveryBanPeerId { peer_id, ip_addr } => {
                self.ban_discovery(peer_id, ip_addr)
            }
            PeerAction::DiscoveryBanIp { ip_addr } => self.ban_ip_discovery(ip_addr),
            PeerAction::PeerAdded(peer_id) => {
                self.queued_messages.push_back(StateAction::PeerAdded(peer_id))
            }
            PeerAction::PeerRemoved(peer_id) => {
                self.queued_messages.push_back(StateAction::PeerRemoved(peer_id))
            }
            PeerAction::BanPeer { .. } | PeerAction::UnBanPeer { .. } => {}
        }
    }

    /// Sends The message to the peer's session and queues in a response.
    ///
    /// Caution: this will replace an already pending response. It's the responsibility of the
    /// caller to select the peer.
    fn handle_block_request(&mut self, peer_id: PeerId, request: BlockRequest) {
        if let Some(ref mut peer) = self.active_peers.get_mut(&peer_id) {
            let (request, response) = match request {
                BlockRequest::GetBlockHeaders(request) => {
                    let (response, rx) = oneshot::channel();
                    let request = PeerRequest::GetBlockHeaders { request, response };
                    let response = PeerResponse::BlockHeaders { response: rx };
                    (request, response)
                }
                BlockRequest::GetBlockBodies(request) => {
                    let (response, rx) = oneshot::channel();
                    let request = PeerRequest::GetBlockBodies { request, response };
                    let response = PeerResponse::BlockBodies { response: rx };
                    (request, response)
                }
                BlockRequest::GetBlockAccessLists(request) => {
                    let (response, rx) = oneshot::channel();
                    let request = PeerRequest::GetBlockAccessLists { request, response };
                    let response = PeerResponse::BlockAccessLists { response: rx };
                    (request, response)
                }
                BlockRequest::GetReceipts(request) => {
                    if peer.capabilities.supports_eth_v70() {
                        let (response, rx) = oneshot::channel();
                        let request = PeerRequest::GetReceipts70 {
                            request: GetReceipts70 {
                                first_block_receipt_index: 0,
                                block_hashes: request.0,
                            },
                            response,
                        };
                        let response = PeerResponse::Receipts70 { response: rx };
                        (request, response)
                    } else if peer.capabilities.supports_eth_v69() {
                        let (response, rx) = oneshot::channel();
                        let request = PeerRequest::GetReceipts69 { request, response };
                        let response = PeerResponse::Receipts69 { response: rx };
                        (request, response)
                    } else {
                        let (response, rx) = oneshot::channel();
                        let request = PeerRequest::GetReceipts { request, response };
                        let response = PeerResponse::Receipts { response: rx };
                        (request, response)
                    }
                }
                BlockRequest::GetSnap(request) => {
                    let (response, rx) = oneshot::channel();
                    let request = PeerRequest::GetSnap { request: *request, response };
                    let response = PeerResponse::Snap { response: rx };
                    (request, response)
                }
            };
            let _ = peer.request_tx.to_session_tx.try_send(request);
            peer.pending_response = Some(response);
        }
    }

    /// Handle the outcome of processed response, for example directly queue another request.
    fn on_block_response_outcome(&mut self, outcome: BlockResponseOutcome) {
        match outcome {
            BlockResponseOutcome::Request(peer, request) => {
                self.handle_block_request(peer, request);
            }
            BlockResponseOutcome::BadResponse(peer, reputation_change) => {
                self.peers_manager.apply_reputation_change(&peer, reputation_change);
            }
        }
    }

    /// Invoked when received a response from a connected peer.
    ///
    /// Delegates the response result to the fetcher which may return an outcome specific
    /// instruction that needs to be handled in [`Self::on_block_response_outcome`]. This could be
    /// a follow-up request or an instruction to slash the peer's reputation.
    fn on_eth_response(&mut self, peer: PeerId, resp: PeerResponseResult) {
        let outcome = match resp {
            PeerResponseResult::BlockHeaders(res) => {
                self.state_fetcher.on_block_headers_response(peer, res)
            }
            PeerResponseResult::BlockBodies(res) => {
                self.state_fetcher.on_block_bodies_response(peer, res)
            }
            PeerResponseResult::Receipts(res) => {
                // Legacy eth/66-68: strip bloom filters and wrap in ReceiptsResponse
                let normalized = res.map(|blocks| {
                    let receipts = blocks
                        .into_iter()
                        .map(|block_receipts| {
                            block_receipts.into_iter().map(|rwb| rwb.receipt).collect()
                        })
                        .collect();
                    ReceiptsResponse::new(receipts)
                });
                self.state_fetcher.on_receipts_response(peer, normalized)
            }
            PeerResponseResult::Receipts69(res) => {
                let normalized = res.map(ReceiptsResponse::new);
                self.state_fetcher.on_receipts_response(peer, normalized)
            }
            PeerResponseResult::Receipts70(res) => {
                let normalized = res.map(ReceiptsResponse::from);
                self.state_fetcher.on_receipts_response(peer, normalized)
            }
            PeerResponseResult::BlockAccessLists(res) => {
                self.state_fetcher.on_block_access_lists_response(peer, res)
            }
            PeerResponseResult::Snap(res) => self.state_fetcher.on_snap_response(peer, res),
            _ => None,
        };

        if let Some(outcome) = outcome {
            self.on_block_response_outcome(outcome);
        }
    }

    /// Advances the state
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<StateAction> {
        loop {
            // drain buffered messages
            if let Some(message) = self.queued_messages.pop_front() {
                return Poll::Ready(message);
            }

            while let Poll::Ready(discovery) = self.discovery.poll(cx) {
                self.on_discovery_event(discovery);
            }

            while let Poll::Ready(action) = self.state_fetcher.poll(cx) {
                match action {
                    FetchAction::BlockRequest { peer_id, request } => {
                        self.handle_block_request(peer_id, request)
                    }
                }
            }

            loop {
                // need to buffer results here to make borrow checker happy
                let mut closed_sessions = Vec::new();
                let mut received_responses = Vec::new();

                // poll all connected peers for responses
                for (id, peer) in &mut self.active_peers {
                    let Some(mut response) = peer.pending_response.take() else { continue };
                    match response.poll(cx) {
                        Poll::Ready(res) => {
                            // check if the error is due to a closed channel to the session
                            if res.err().is_some_and(|err| err.is_channel_closed()) {
                                debug!(
                                    target: "net",
                                    ?id,
                                    "Request canceled, response channel from session closed."
                                );
                                // if the channel is closed, this means the peer session is also
                                // closed, in which case we can invoke the
                                // [Self::on_closed_session]
                                // immediately, preventing followup requests and propagate the
                                // connection dropped error
                                closed_sessions.push(*id);
                            } else {
                                received_responses.push((*id, res));
                            }
                        }
                        Poll::Pending => {
                            // not ready yet, store again.
                            peer.pending_response = Some(response);
                        }
                    };
                }

                for peer in closed_sessions {
                    self.on_session_closed(peer)
                }

                if received_responses.is_empty() {
                    break;
                }

                for (peer_id, resp) in received_responses {
                    self.on_eth_response(peer_id, resp);
                }
            }

            // poll peer manager
            while let Poll::Ready(action) = self.peers_manager.poll(cx) {
                self.on_peer_action(action);
            }

            // We need to poll again in case we have received any responses because they may have
            // triggered follow-up requests.
            if self.queued_messages.is_empty() {
                return Poll::Pending;
            }
        }
    }
}

/// Tracks the state of a Peer with an active Session.
///
/// For example known blocks,so we can decide what to announce.
#[derive(Debug)]
pub(crate) struct ActivePeer {
    /// The capabilities of the remote peer.
    pub(crate) capabilities: Arc<Capabilities>,
    /// A communication channel directly to the session task.
    pub(crate) request_tx: PeerRequestSender<PeerRequest>,
    /// The response receiver for a currently active request to that peer.
    pub(crate) pending_response: Option<PeerResponse>,
}

/// Everything [`NetworkState::on_session_activated`] needs to register a newly established
/// session.
pub(crate) struct SessionActivation {
    /// The remote peer's identifier.
    pub(crate) peer: PeerId,
    /// The capabilities the peer announced.
    pub(crate) capabilities: Arc<Capabilities>,
    /// A communication channel directly to the session task.
    pub(crate) request_tx: PeerRequestSender<PeerRequest>,
    /// The maximum time the session waits for a response from the peer.
    pub(crate) timeout: Arc<AtomicU64>,
    /// The range info for the peer.
    pub(crate) range_info: Option<BlockRangeInfo>,
    /// Whether the connection negotiated `snap/2` and can serve [`PeerRequest::GetSnap`].
    pub(crate) supports_snap: bool,
}

/// Message variants triggered by the [`NetworkState`]
#[derive(Debug)]
pub(crate) enum StateAction {
    /// Create a new connection to the given node.
    Connect { remote_addr: SocketAddr, peer_id: PeerId },
    /// Disconnect an existing connection
    Disconnect {
        peer_id: PeerId,
        /// Why the disconnect was initiated
        reason: Option<DisconnectReason>,
    },
    /// Retrieved a [`ForkId`] from the peer via ENR request, See <https://eips.ethereum.org/EIPS/eip-868>
    DiscoveredEnrForkId {
        peer_id: PeerId,
        /// The address of the peer.
        addr: PeerAddr,
        /// The reported [`ForkId`] by this peer.
        fork_id: ForkId,
    },
    /// A new node was found through the discovery, possibly with a `ForkId`
    DiscoveredNode { peer_id: PeerId, addr: PeerAddr, fork_id: Option<ForkId> },
    /// A peer was added
    PeerAdded(PeerId),
    /// A peer was dropped
    PeerRemoved(PeerId),
}

#[cfg(test)]
mod tests {
    use std::{
        future::poll_fn,
        sync::{Arc, atomic::AtomicU64},
    };

    use alloy_primitives::B256;
    use base_common_types_chain::{BaseBlockBody as BlockBody, Header};
    use base_execution_network_wire::{
        BlockBodies, BodiesClient, Capabilities, Capability, EthVersion, PeerId, RequestError,
    };
    use tokio::sync::mpsc;
    use tokio_stream::{StreamExt, wrappers::ReceiverStream};

    use crate::{
        PeerRequest, PeerRequestSender,
        discovery::Discovery,
        fetch::StateFetcher,
        peers::PeersManager,
        state::{NetworkState, SessionActivation},
    };

    /// Returns a testing instance of the [`NetworkState`].
    fn state() -> NetworkState {
        let peers = PeersManager::default();
        let handle = peers.handle();
        NetworkState {
            active_peers: Default::default(),
            peers_manager: Default::default(),
            queued_messages: Default::default(),
            discovery: Discovery::noop(),
            state_fetcher: StateFetcher::new(handle, Default::default()),
        }
    }

    fn capabilities() -> Arc<Capabilities> {
        Arc::new(vec![Capability::from(EthVersion::Eth67)].into())
    }

    // tests that ongoing requests are answered with connection dropped if the session that received
    // that request is drops the request object.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_dropped_active_session() {
        let mut state = state();
        let client = state.fetch_client();

        let peer_id = PeerId::random();
        let (tx, session_rx) = mpsc::channel(1);
        let peer_tx = PeerRequestSender::new(peer_id, tx);

        state.on_session_activated(SessionActivation {
            peer: peer_id,
            capabilities: capabilities(),
            request_tx: peer_tx,
            timeout: Arc::new(AtomicU64::new(1)),
            range_info: None,
            supports_snap: false,
        });

        assert!(state.active_peers.contains_key(&peer_id));

        let body = BlockBody { ommers: vec![Header::default()], ..Default::default() };

        let body_response = body.clone();

        // this mimics an active session that receives the requests from the state
        tokio::task::spawn(async move {
            let mut stream = ReceiverStream::new(session_rx);
            let resp = stream.next().await.unwrap();
            match resp {
                PeerRequest::GetBlockBodies { response, .. } => {
                    response.send(Ok(BlockBodies(vec![body_response]))).unwrap();
                }
                _ => unreachable!(),
            }

            // wait for the next request, then drop
            let _resp = stream.next().await.unwrap();
        });

        // spawn the state as future
        tokio::task::spawn(async move {
            loop {
                poll_fn(|cx| state.poll(cx)).await;
            }
        });

        // send requests to the state via the client
        let (peer, bodies) = client.get_block_bodies(vec![B256::random()]).await.unwrap().split();
        assert_eq!(peer, peer_id);
        assert_eq!(bodies, vec![body]);

        let resp = client.get_block_bodies(vec![B256::random()]).await;
        assert!(resp.is_err());
        assert_eq!(resp.unwrap_err(), RequestError::ConnectionDropped);
    }
}
