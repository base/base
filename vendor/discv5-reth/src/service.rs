//! The Discovery v5 protocol. See `lib.rs` for further details.
//!
//! Note: Discovered ENR's are not automatically added to the routing table. Only established
//! sessions get added, ensuring only valid ENRs are added. Manual additions can be made using the
//! `add_enr()` function.
//!
//! Response to queries return `PeerId`. Only the trusted (a session has been established with)
//! `PeerId`'s are returned, as ENR's for these `PeerId`'s are stored in the routing table and as
//! such should have an address to connect to. Untrusted `PeerId`'s can be obtained from the
//! `Service::Discovered` event, which is fired as peers get discovered.
//!
//! Note that although the ENR crate does support Ed25519 keys, these are currently not
//! supported as the ECDH procedure isn't specified in the specification. Therefore, only
//! secp256k1 keys are supported currently.

use std::{
    collections::HashMap,
    convert::TryInto,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    task::Poll,
    time::Instant,
};

use delay_map::HashSetDelay;
use enr::{CombinedKey, NodeId};
use fnv::FnvHashMap;
use futures::prelude::*;
use more_asserts::debug_unreachable;
use parking_lot::RwLock;
use rpc::*;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use crate::{
    Config, Enr, Event, IpMode,
    error::{RequestError, ResponseError},
    handler::{Handler, HandlerIn, HandlerOut},
    kbucket::{
        self, ConnectionDirection, ConnectionState, FailureReason, InsertResult, KBucketsTable,
        MAX_NODES_PER_BUCKET, NodeStatus, UpdateResult,
    },
    node_info::{NodeAddress, NodeContact, NonContactable},
    packet::MAX_PACKET_SIZE,
    query_pool::{
        FindNodeQueryConfig, PredicateQueryConfig, QueryId, QueryPool, QueryPoolState, TargetKey,
    },
    rpc,
};

mod connectivity_state;
pub use connectivity_state::{
    ConnectivityState, DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT, TimerFailure,
};
mod ip_vote;
pub use ip_vote::IpVote;
mod query_info;
pub use query_info::{QueryInfo, QueryType};
#[cfg(test)]
pub mod test;

/// The number of distances (buckets) we simultaneously request from each peer.
/// NOTE: This must not be larger than 127.
pub(crate) const DISTANCES_TO_REQUEST_PER_PEER: usize = 3;

/// Currently, a maximum of `DISTANCES_TO_REQUEST_PER_PEER * BUCKET_SIZE` peers
/// can be returned. Datagrams have a max size of 1280 and ENR's have a max size
/// of 300 bytes. Bucket sizes should be 16. Therefore, to return all required peers
/// there should be no more than `5 * DISTANCES_TO_REQUEST_PER_PEER` responses.
pub(crate) const MAX_NODES_RESPONSES: usize =
    (MAX_NODES_PER_BUCKET / 4 + 1) * DISTANCES_TO_REQUEST_PER_PEER;

/// Request type for Protocols using `TalkReq` message.
///
/// Automatically responds with an empty body on drop if
/// [`TalkRequest::respond`] is not called.
#[derive(Debug)]
pub struct TalkRequest {
    id: RequestId,
    node_address: NodeAddress,
    protocol: Vec<u8>,
    body: Vec<u8>,
    sender: Option<mpsc::UnboundedSender<HandlerIn>>,
}

impl Drop for TalkRequest {
    fn drop(&mut self) {
        let sender = match self.sender.take() {
            Some(s) => s,
            None => return,
        };

        let response =
            Response { id: self.id.clone(), body: ResponseBody::Talk { response: vec![] } };

        debug!(node_address = %self.node_address, "Sending empty TALK response");
        if let Err(e) =
            sender.send(HandlerIn::Response(self.node_address.clone(), Box::new(response)))
        {
            warn!(error = %e,"Failed to send empty talk response")
        }
    }
}

impl TalkRequest {
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_address.node_id
    }

    pub fn protocol(&self) -> &[u8] {
        &self.protocol
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn respond(mut self, response: Vec<u8>) -> Result<(), ResponseError> {
        debug!(node_address = %self.node_address, "Sending TALK response");

        let response = Response { id: self.id.clone(), body: ResponseBody::Talk { response } };

        self.sender
            .take()
            .unwrap()
            .send(HandlerIn::Response(self.node_address.clone(), Box::new(response)))
            .map_err(|_| ResponseError::ChannelClosed)?;

        Ok(())
    }
}

/// The types of requests to send to the Discv5 service.
#[derive(Debug)]
pub enum ServiceRequest {
    /// A request to start a query. There are two types of queries:
    /// - A FindNode Query - Searches for peers using a random target.
    /// - A Predicate Query - Searches for peers closest to a random target that match a specified
    ///   predicate.
    StartQuery(QueryKind, oneshot::Sender<Vec<Enr>>),
    /// Send a FINDNODE request for nodes that fall within the given set of distances,
    /// to the designated peer and wait for a response.
    FindNodeDesignated(NodeContact, Vec<u64>, oneshot::Sender<Result<Vec<Enr>, RequestError>>),
    /// The TALK discv5 RPC function.
    Talk(NodeContact, Vec<u8>, Vec<u8>, oneshot::Sender<Result<Vec<u8>, RequestError>>),
    /// The PING discv5 RPC function.
    Ping(Enr, Option<oneshot::Sender<Result<Pong, RequestError>>>),
    /// Sets up an event stream where the discv5 server will return various events such as
    /// discovered nodes as it traverses the DHT.
    RequestEventStream(oneshot::Sender<mpsc::Receiver<Event>>),
}

use crate::discv5::PERMIT_BAN_LIST;

pub struct Service {
    /// Configuration parameters.
    config: Config,
    /// The local ENR of the server.
    local_enr: Arc<RwLock<Enr>>,
    /// The key associated with the local ENR.
    enr_key: Arc<RwLock<CombinedKey>>,
    /// Storage of the ENR record for each node.
    kbuckets: Arc<RwLock<KBucketsTable<NodeId, Enr>>>,
    /// All the iterative queries we are currently performing.
    queries: QueryPool<QueryInfo, NodeId, Enr>,
    /// RPC requests that have been sent and are awaiting a response. Some requests are linked to a
    /// query.
    active_requests: FnvHashMap<RequestId, ActiveRequest>,
    /// Keeps track of the number of responses received from a NODES response.
    active_nodes_responses: HashMap<RequestId, NodesResponse>,
    /// A map of votes nodes have made about our external IP address. We accept the majority.
    ip_votes: Option<IpVote>,
    /// The channel to send messages to the handler.
    handler_send: mpsc::UnboundedSender<HandlerIn>,
    /// The channel to receive messages from the handler.
    handler_recv: mpsc::Receiver<HandlerOut>,
    /// The exit channel to shutdown the handler.
    handler_exit: Option<oneshot::Sender<()>>,
    /// The channel of messages sent by the controlling discv5 wrapper.
    discv5_recv: mpsc::Receiver<ServiceRequest>,
    /// The exit channel for the service.
    exit: oneshot::Receiver<()>,
    /// A queue of peers that require regular ping to check connectivity.
    peers_to_ping: HashSetDelay<NodeId>,
    /// A channel that the service emits events on.
    event_stream: Option<mpsc::Sender<Event>>,
    /// Type of socket we are using
    ip_mode: IpMode,
    /// This stores information about whether we think we have open ports and if we are externally
    /// contactable or not. This decides if we should update our ENR or set it to None, if we are
    /// not contactable.
    connectivity_state: ConnectivityState,
}

/// Active RPC request awaiting a response from the handler.
#[derive(Debug)]
struct ActiveRequest {
    /// The address the request was sent to.
    pub contact: NodeContact,
    /// The request that was sent.
    pub request_body: RequestBody,
    /// The query ID if the request was related to a query.
    pub query_id: Option<QueryId>,
    /// Channel callback if this request was from a user level request.
    pub callback: Option<CallbackResponse>,
}

#[derive(Debug)]
pub struct Pong {
    /// The current ENR sequence number of the responder.
    pub enr_seq: u64,
    /// Our external IP address as observed by the responder.
    pub ip: IpAddr,
    /// Our external UDP port as observed by the responder.
    pub port: u16,
}

/// The kinds of responses we can send back to the discv5 layer.
#[derive(Debug)]
pub enum CallbackResponse {
    /// A response to a requested Nodes.
    Nodes(oneshot::Sender<Result<Vec<Enr>, RequestError>>),
    /// A response from a TALK request
    Talk(oneshot::Sender<Result<Vec<u8>, RequestError>>),
    /// A response from a Pong request
    Pong(oneshot::Sender<Result<Pong, RequestError>>),
}

/// For multiple responses to a FindNodes request, this keeps track of the request count
/// and the nodes that have been received.
#[derive(Debug)]
struct NodesResponse {
    /// The response count.
    count: usize,
    /// The filtered nodes that have been received.
    received_nodes: Vec<Enr>,
}

impl Default for NodesResponse {
    fn default() -> Self {
        NodesResponse { count: 1, received_nodes: Vec::new() }
    }
}

impl core::fmt::Debug for Service {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Service")
            .field("config", &self.config)
            .field("local_enr", &self.local_enr)
            .field("kbuckets", &self.kbuckets)
            .field("queries", &self.queries)
            .field("active_requests", &self.active_requests)
            .field("active_nodes_responses", &self.active_nodes_responses)
            .field("ip_votes", &self.ip_votes)
            .field("handler_send", &self.handler_send)
            .field("handler_recv", &self.handler_recv)
            .field("handler_exit", &self.handler_exit)
            .field("discv5_recv", &self.discv5_recv)
            .field("exit", &self.exit)
            .field("peers_to_ping", &self.peers_to_ping)
            .field("event_stream", &self.event_stream)
            .field("ip_mode", &self.ip_mode)
            .field("connectivity_state", &self.connectivity_state)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// Builds the `Service` main struct.
    ///
    /// `local_enr` is the `ENR` representing the local node. This contains node identifying information, such
    /// as IP addresses and ports which we wish to broadcast to other nodes via this discovery
    /// mechanism.
    pub async fn spawn(
        local_enr: Arc<RwLock<Enr>>,
        enr_key: Arc<RwLock<CombinedKey>>,
        kbuckets: Arc<RwLock<KBucketsTable<NodeId, Enr>>>,
        config: Config,
    ) -> Result<(oneshot::Sender<()>, mpsc::Sender<ServiceRequest>), std::io::Error> {
        // process behaviour-level configuration parameters
        let ip_votes = if config.enr_update {
            Some(IpVote::new(config.enr_peer_update_min, config.vote_duration))
        } else {
            None
        };

        let ip_mode = IpMode::new_from_listen_config(&config.listen_config);

        // build the session service
        let (handler_exit, handler_send, handler_recv) =
            Handler::spawn(local_enr.clone(), enr_key.clone(), config.clone()).await?;

        // create the required channels
        let (discv5_send, discv5_recv) = mpsc::channel(30);
        let (exit_send, exit) = oneshot::channel();

        let connectivity_state = ConnectivityState::new(config.auto_nat_listen_duration);

        config.executor.clone().expect("Executor must be present").spawn(Box::pin(async move {
            let mut service = Service {
                local_enr,
                enr_key,
                kbuckets,
                queries: QueryPool::new(config.query_timeout),
                active_requests: Default::default(),
                active_nodes_responses: HashMap::new(),
                ip_votes,
                handler_send,
                handler_recv,
                handler_exit: Some(handler_exit),
                peers_to_ping: HashSetDelay::new(config.ping_interval),
                discv5_recv,
                event_stream: None,
                exit,
                config: config.clone(),
                ip_mode,
                connectivity_state,
            };

            info!(mode = ?service.ip_mode, "Discv5 Service started");
            service.start().await;
        }));

        Ok((exit_send, discv5_send))
    }

    /// The main execution loop of the discv5 serviced.
    async fn start(&mut self) {
        loop {
            tokio::select! {
                _ = &mut self.exit => {
                    if let Some(exit) = self.handler_exit.take() {
                        let _ = exit.send(());
                        info!("Discv5 Service shutdown");
                    }
                    return;
                }
                Some(service_request) = self.discv5_recv.recv() => {
                    match service_request {
                        ServiceRequest::StartQuery(query, callback) => {
                            match query {
                                QueryKind::FindNode { target_node } => {
                                    self.start_findnode_query(target_node, callback);
                                }
                                QueryKind::Predicate { target_node, target_peer_no, predicate } => {
                                    self.start_predicate_query(target_node, target_peer_no, predicate, callback);
                                }
                            }
                        }
                        ServiceRequest::FindNodeDesignated(node_contact, distance, callback) => {
                            self.request_find_node_designated_peer(node_contact, distance, Some(callback));
                        }
                        ServiceRequest::Talk(node_contact, protocol, request, callback) => {
                            self.talk_request(node_contact, protocol, request, callback);
                        }
                        ServiceRequest::Ping(enr, callback) => {
                            self.send_ping(enr, callback);
                        }
                        ServiceRequest::RequestEventStream(callback) => {
                            // the channel size needs to be large to handle many discovered peers
                            // if we are reporting them on the event stream.
                            let channel_size = if self.config.report_discovered_peers { 100 } else { 30 };
                            let (event_stream, event_stream_recv) = mpsc::channel(channel_size);
                            self.event_stream = Some(event_stream);
                            if callback.send(event_stream_recv).is_err() {
                                error!("Failed to return the event stream channel");
                            }
                        }
                    }
                }
                Some(event) = self.handler_recv.recv() => {
                    match event {
                        HandlerOut::Established(enr, socket_addr, direction) => {
                            self.inject_session_established(enr.clone(), &socket_addr, direction);
                            self.send_event(Event::SessionEstablished(enr, socket_addr));
                        }
                        HandlerOut::Request(node_address, request) => {
                                self.handle_rpc_request(node_address, *request);
                            }
                        HandlerOut::Response(node_address, response) => {
                                self.handle_rpc_response(node_address, *response);
                            }
                        HandlerOut::WhoAreYou(whoareyou_ref) => {
                            // check what our latest known ENR is for this node.
                            if let Some(known_enr) = self.find_enr(&whoareyou_ref.0.node_id) {
                                if let Err(e) = self.handler_send.send(HandlerIn::WhoAreYou(whoareyou_ref, Some(known_enr))) {
                                    warn!(error = %e, "Failed to send whoareyou");
                                };
                            } else {
                                // do not know of this peer
                                debug!(node_address = %whoareyou_ref.0, "NodeId unknown, requesting ENR.");
                                if let Err(e) = self.handler_send.send(HandlerIn::WhoAreYou(whoareyou_ref, None)) {
                                    warn!(error = %e, "Failed to send who are you to unknown enr peer");
                                }
                            }
                        }
                        HandlerOut::RequestFailed(request_id, error) => {
                            if let RequestError::Timeout = error {
                                debug!(id = %request_id, "RPC Request timed out");
                            } else {
                                warn!(id = %request_id, ?error, "RPC Request failed");
                            }
                            self.rpc_failure(request_id, error);
                        }
                        HandlerOut::UnverifiableEnr{enr, socket, node_id} => {
                            // We have received an ENR that has incorrect socket address fields
                            // (given the source of the pakcet we received.
                            // If this node exists in our routing table, remove it, as it may have shifted
                            // ip addresses and is now no longer contactable.
                            let key = kbucket::Key::from(node_id);
                            if self.kbuckets.write().remove(&key) {
                                debug!(?node_id, "Uncontactable node removed from routing table");
                            }
                            self.send_event(Event::UnverifiableEnr{enr, socket, node_id});
                        }
                        HandlerOut::UnrecognizedFrame(frame) => {
                            self.send_event(Event::UnrecognizedFrame(frame));
                        }
                        HandlerOut::ExpiredSessions(expired_sessions) => {
                            self.send_event(Event::SessionsExpired(expired_sessions));
                        }
                    }
                }
                event = Service::bucket_maintenance_poll(&self.kbuckets) => {
                    self.send_event(event);
                }
                query_event = Service::query_event_poll(&mut self.queries) => {
                    match query_event {
                        QueryEvent::Waiting(query_id, node_id, request_body) => {
                            self.send_rpc_query(query_id, node_id, request_body);
                        }
                        // Note: Currently the distinction between a timed-out query and a finished
                        // query is superfluous, however it may be useful in future versions.
                        QueryEvent::Finished(query) | QueryEvent::TimedOut(query) => {
                            let id = query.id();
                            let mut result = query.into_result();
                            // obtain the ENR's for the resulting nodes
                            let mut found_enrs = Vec::new();
                            for node_id in result.closest_peers {
                                if let Some(position) = result.target.untrusted_enrs.iter().position(|enr| enr.node_id() == node_id) {
                                    let enr = result.target.untrusted_enrs.swap_remove(position);
                                    found_enrs.push(enr);
                                } else if let Some(enr) = self.find_enr(&node_id) {
                                    // look up from the routing table
                                    found_enrs.push(enr);
                                }
                                else {
                                    warn!("ENR not present in queries results");
                                }
                            }
                            if result.target.callback.send(found_enrs).is_err() {
                                warn!(query_id = *id, "Callback dropped for query. Results dropped");
                            }
                        }
                    }
                }
                Some(Ok(node_id)) = self.peers_to_ping.next() => {
                    // If the node is in the routing table, Ping it and re-queue the node.
                    let key = kbucket::Key::from(node_id);
                    let enr =  {
                        if let kbucket::Entry::Present(entry, _) = self.kbuckets.write().entry(&key) {
                        // The peer is in the routing table, ping it and re-queue the ping
                        self.peers_to_ping.insert(node_id);
                        Some(entry.value().clone())
                        } else { None }
                    };

                    if let Some(enr) = enr {
                        self.send_ping(enr, None);
                    }
                }
                connectivity_timeout = self.connectivity_state.poll() => {
                    let updated_enr = match connectivity_timeout {
                        TimerFailure::V4 => {
                            // We have not received enough incoming connections in the required
                            // time. Remove our ENR advertisement.
                            info!(ip_version="v4", next_attempt_in=%DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT.as_secs(), "UDP Socket removed from ENR");
                            if let Err(error) = self.local_enr.write().remove_udp_socket(&self.enr_key.read()) {
                                error!(?error, "Failed to update the ENR");
                                false
                            } else {
                                // ENR was updated
                                true
                            }
                        }
                        TimerFailure::V6 => {
                            // We have not received enough incoming connections in the required
                            // time. Remove our ENR advertisement.
                            info!(ip_version="v6", next_attempt_in=%DURATION_UNTIL_NEXT_CONNECTIVITY_ATTEMPT.as_secs(), "UDP Socket removed from ENR");
                            if let Err(error) = self.local_enr.write().remove_udp6_socket(&self.enr_key.read()) {
                                error!(?error, "Failed to update the ENR");
                                false
                            } else {
                                // ENR was updated
                                true
                            }
                        }
                    };
                    if updated_enr {
                        // Inform our known peers of our updated ENR
                        self.ping_connected_peers();
                    }
                }
            }
        }
    }

    /// Internal function that starts a query.
    fn start_findnode_query(&mut self, target_node: NodeId, callback: oneshot::Sender<Vec<Enr>>) {
        let mut target = QueryInfo {
            query_type: QueryType::FindNode(target_node),
            untrusted_enrs: Default::default(),
            distances_to_request: DISTANCES_TO_REQUEST_PER_PEER,
            callback,
        };

        let target_key: kbucket::Key<NodeId> = target.key();
        let mut known_closest_peers = Vec::new();
        {
            let mut kbuckets = self.kbuckets.write();
            for closest in kbuckets.closest_values(&target_key) {
                // Add the known ENR's to the untrusted list
                target.untrusted_enrs.push(closest.value);
                // Add the key to the list for the query
                known_closest_peers.push(closest.key);
            }
        }

        if known_closest_peers.is_empty() {
            warn!("No known_closest_peers found. Return empty result without sending query.");
            if target.callback.send(vec![]).is_err() {
                warn!("Failed to callback");
            }
        } else {
            let query_config = FindNodeQueryConfig::new_from_config(&self.config);
            self.queries.add_findnode_query(query_config, target, known_closest_peers);
        }
    }

    /// Internal function that starts a query.
    fn start_predicate_query(
        &mut self,
        target_node: NodeId,
        num_nodes: usize,
        predicate: Box<dyn Fn(&Enr) -> bool + Send>,
        callback: oneshot::Sender<Vec<Enr>>,
    ) {
        let mut target = QueryInfo {
            query_type: QueryType::FindNode(target_node),
            untrusted_enrs: Default::default(),
            distances_to_request: DISTANCES_TO_REQUEST_PER_PEER,
            callback,
        };

        let target_key: kbucket::Key<NodeId> = target.key();

        // Map the TableEntry to an ENR.
        let kbucket_predicate = |e: &Enr| predicate(e);

        let mut known_closest_peers = Vec::<kbucket::PredicateKey<_>>::new();
        {
            let mut kbuckets = self.kbuckets.write();
            for closest in kbuckets.closest_values_predicate(&target_key, &kbucket_predicate) {
                let (node_id_predicate, enr) = closest.to_key_value();
                // Add the known ENR's to the untrusted list
                target.untrusted_enrs.push(enr);
                // Add the key to the list for the query
                known_closest_peers.push(node_id_predicate);
            }
        };

        if known_closest_peers.is_empty() {
            warn!("No known_closest_peers found. Return empty result without sending query.");
            if target.callback.send(vec![]).is_err() {
                warn!("Failed to callback");
            }
        } else {
            let mut query_config = PredicateQueryConfig::new_from_config(&self.config);
            query_config.num_results = num_nodes;
            self.queries.add_predicate_query(query_config, target, known_closest_peers, predicate);
        }
    }

    /// Returns an ENR if one is known for the given NodeId.
    pub fn find_enr(&self, node_id: &NodeId) -> Option<Enr> {
        // check if we know this node id in our routing table
        let key = kbucket::Key::from(*node_id);
        if let kbucket::Entry::Present(entry, _) = self.kbuckets.write().entry(&key) {
            return Some(entry.value().clone());
        }
        // check the untrusted addresses for ongoing queries
        for query in self.queries.iter() {
            if let Some(enr) =
                query.target().untrusted_enrs.iter().find(|v| v.node_id() == *node_id)
            {
                return Some(enr.clone());
            }
        }
        None
    }

    /// Processes an RPC request from a peer. Requests respond to the received socket address,
    /// rather than the IP of the known ENR.
    fn handle_rpc_request(&mut self, node_address: NodeAddress, req: Request) {
        let id = req.id;
        match req.body {
            RequestBody::FindNode { distances } => {
                self.send_nodes_response(node_address, id, distances);
            }
            RequestBody::Ping { enr_seq } => {
                // check if we need to update the known ENR
                let mut to_request_enr = None;
                match self.kbuckets.write().entry(&node_address.node_id.into()) {
                    kbucket::Entry::Present(ref entry, _) if entry.value().seq() < enr_seq => {
                        let enr = entry.value().clone();
                        to_request_enr = Some(enr);
                    }
                    kbucket::Entry::Pending(ref entry, _) if entry.value().seq() < enr_seq => {
                        let enr = entry.value().clone();
                        to_request_enr = Some(enr);
                    }
                    // don't know the peer, don't request its most recent ENR
                    _ => {}
                }
                if let Some(enr) = to_request_enr {
                    match NodeContact::try_from_enr(enr, self.ip_mode) {
                        Ok(contact) => {
                            self.request_find_node_designated_peer(contact, vec![0], None);
                        }
                        Err(NonContactable { enr }) => {
                            debug_unreachable!("Stored ENR is not contactable. {}", enr);
                            error!(
                                %enr,
                                "Stored ENR is not contactable! This should never happen",
                            );
                        }
                    }
                }

                // build the PONG response
                let src = node_address.socket_addr;
                if let Ok(port) = src.port().try_into() {
                    let response = Response {
                        id,
                        body: ResponseBody::Pong {
                            enr_seq: self.local_enr.read().seq(),
                            ip: src.ip(),
                            port,
                        },
                    };
                    debug!(%node_address, "Sending PONG response");
                    if let Err(e) = self
                        .handler_send
                        .send(HandlerIn::Response(node_address, Box::new(response)))
                    {
                        warn!(error = %e, "Failed to send response");
                    }
                } else {
                    warn!(%src, "The src port number should be non zero");
                }
            }
            RequestBody::Talk { protocol, request } => {
                let req = TalkRequest {
                    id,
                    node_address,
                    protocol,
                    body: request,
                    sender: Some(self.handler_send.clone()),
                };

                self.send_event(Event::TalkRequest(req));
            }
        }
    }

    /// Processes an RPC response from a peer.
    fn handle_rpc_response(&mut self, node_address: NodeAddress, response: Response) {
        // verify we know of the rpc_id
        let id = response.id.clone();

        let Some(mut active_request) = self.active_requests.remove(&id) else {
            warn!(%id, "Received an RPC response which doesn't match a request");
            return;
        };

        debug!(
            response = %response.body,
            request = %active_request.request_body,
            from = %active_request.contact,
            "Received RPC response",
        );
        // Check that the responder matches the expected request

        let expected_node_address = active_request.contact.node_address();
        if expected_node_address != node_address {
            debug_unreachable!("Handler returned a response not matching the used socket addr");
            return error!(
                expected = %expected_node_address,
                received = %node_address,
                request_id = %id,
                "Received a response from an unexpected address",
            );
        }

        if !response.match_request(&active_request.request_body) {
            warn!(
                %node_address,
                "Node gave an incorrect response type. Ignoring response"
            );
            return;
        }

        let node_id = node_address.node_id;

        match response.body {
            ResponseBody::Nodes { total, mut nodes } => {
                if total > MAX_NODES_RESPONSES as u64 {
                    warn!(
                        total,
                        "NodesResponse has a total larger than {}, nodes will be truncated",
                        MAX_NODES_RESPONSES
                    );
                }

                // These are sanitized and ordered
                let distances_requested = match &active_request.request_body {
                    RequestBody::FindNode { distances } => distances,
                    _ => unreachable!(),
                };

                if let Some(CallbackResponse::Nodes(callback)) = active_request.callback.take() {
                    if let Err(e) = callback.send(Ok(nodes)) {
                        warn!(error = ?e, "Failed to send response in callback")
                    }
                    return;
                }

                // Filter out any nodes that are not of the correct distance
                let peer_key: kbucket::Key<NodeId> = node_id.into();

                // The distances we send are sanitized an ordered.
                // We never send an ENR request in combination of other requests.
                if distances_requested.len() == 1 && distances_requested[0] == 0 {
                    // we requested an ENR update
                    if nodes.len() > 1 {
                        warn!(
                            %node_address,
                            "Peer returned more than one ENR for itself. Blacklisting",
                        );
                        let ban_timeout = self.config.ban_duration.map(|v| Instant::now() + v);
                        PERMIT_BAN_LIST.write().ban(node_address, ban_timeout);
                        nodes.retain(|enr| peer_key.log2_distance(&enr.node_id().into()).is_none());
                    }
                } else {
                    let before_len = nodes.len();
                    nodes.retain(|enr| {
                        peer_key
                            .log2_distance(&enr.node_id().into())
                            .map(|distance| distances_requested.contains(&distance))
                            .unwrap_or_else(|| false)
                    });

                    if nodes.len() < before_len {
                        // Peer sent invalid ENRs. Blacklist the Node
                        let node_id = active_request.contact.node_id();
                        let addr = active_request.contact.socket_addr();
                        warn!(%node_id, %addr, "ENRs received of unsolicited distances. Blacklisting");
                        let ban_timeout = self.config.ban_duration.map(|v| Instant::now() + v);
                        PERMIT_BAN_LIST.write().ban(node_address, ban_timeout);
                    }
                }

                // handle the case that there is more than one response
                if total > 1 {
                    let mut current_response =
                        self.active_nodes_responses.remove(&id).unwrap_or_default();

                    debug!("Nodes Response: {} of {} received", current_response.count, total);
                    // If there are more requests coming, store the nodes and wait for
                    // another response
                    // If we have already received all our required nodes, drop any extra
                    // rpc messages.
                    if current_response.received_nodes.len() < self.config.max_nodes_response
                        && (current_response.count as u64) < total
                        && current_response.count < MAX_NODES_RESPONSES
                    {
                        current_response.count += 1;

                        current_response.received_nodes.append(&mut nodes);
                        self.active_nodes_responses.insert(id.clone(), current_response);
                        self.active_requests.insert(id, active_request);
                        return;
                    }

                    // have received all the Nodes responses we are willing to accept
                    // ignore duplicates here as they will be handled when adding
                    // to the DHT
                    current_response.received_nodes.append(&mut nodes);
                    nodes = current_response.received_nodes;
                }

                debug!(
                    len = nodes.len(),
                    total,
                    from = %active_request.contact,
                    "Received a nodes response",
                );
                // note: If a peer sends an initial NODES response with a total > 1 then
                // in a later response sends a response with a total of 1, all previous nodes
                // will be ignored.
                // ensure any mapping is removed in this rare case
                self.active_nodes_responses.remove(&id);

                self.discovered(&node_id, nodes, active_request.query_id);
            }
            ResponseBody::Pong { enr_seq, ip, port } => {
                // Send the response to the user, if they are who asked
                if let Some(CallbackResponse::Pong(callback)) = active_request.callback {
                    let response = Pong { enr_seq, ip, port: port.get() };
                    if let Err(e) = callback.send(Ok(response)) {
                        warn!(error = ?e, "Failed to send callback response")
                    };
                    return;
                }

                let socket = SocketAddr::new(ip, port.get());
                // Register the vote, this counts towards potentially updating the ENR for external
                // advertisement
                self.handle_ip_vote_from_pong(node_id, socket);

                // check if we need to request a new ENR
                if let Some(enr) = self.find_enr(&node_id) {
                    if enr.seq() < enr_seq {
                        // request an ENR update
                        debug!(from = %active_request.contact, "Requesting an ENR update");
                        let request_body = RequestBody::FindNode { distances: vec![0] };
                        let active_request = ActiveRequest {
                            contact: active_request.contact,
                            request_body,
                            query_id: None,
                            callback: None,
                        };
                        self.send_rpc_request(active_request);
                    }
                    // Only update the routing table if the new ENR is contactable
                    if self.ip_mode.get_contactable_addr(&enr).is_some() {
                        self.connection_updated(node_id, ConnectionStatus::PongReceived);
                    }
                }
            }
            ResponseBody::Talk { response } => {
                // Send the response to the user
                match active_request.callback {
                    Some(CallbackResponse::Talk(callback)) => {
                        if let Err(e) = callback.send(Ok(response)) {
                            warn!(error = ?e, "Failed to send callback response")
                        };
                    }
                    _ => error!("Invalid callback for response"),
                }
            }
        }
    }

    // We have received a PONG which informs us for our external socket. This function decides
    // how we should handle this vote and whether or not to update our ENR. This is done on a
    // majority-based voting system, see `IpVote` for more details.
    fn handle_ip_vote_from_pong(&mut self, node_id: NodeId, socket: SocketAddr) {
        // Check that we are in a state to handle any IP votes
        if !self.connectivity_state.should_count_ip_vote(&socket) {
            return;
        }

        // We are configured to ignore ENR updates, we therefore ignore PONGs.
        if self.ip_votes.is_none() {
            return;
        }

        // Only count votes that are from peers we have contacted.
        let key: kbucket::Key<NodeId> = node_id.into();
        let is_connected_and_outgoing = matches!(
                        self.kbuckets.write().entry(&key),
                        kbucket::Entry::Present(_, status)
                            if status.is_connected() && !status.is_incoming());

        // Check to make sure this is an outgoing peer vote, otherwise if we need the vote due to a
        // lack of majority, we accept it.
        if !(is_connected_and_outgoing | self.require_more_ip_votes(socket.is_ipv6())) {
            return;
        }

        match socket {
            SocketAddr::V4(_) => {
                let local_ip4_socket = self.local_enr.read().udp4_socket();
                if let Some(ip_votes) = self.ip_votes.as_mut() {
                    ip_votes.insert(node_id, socket);
                    let maybe_ip4_majority = ip_votes.majority().0;

                    let new_ip4 = maybe_ip4_majority.and_then(|majority| {
                        if Some(majority) != local_ip4_socket { Some(majority) } else { None }
                    });

                    // If we have a new ipv4 majority
                    if let Some(new_ip4) = new_ip4 {
                        let new_ip4: SocketAddr = new_ip4.into();
                        let result =
                            self.local_enr.write().set_udp_socket(new_ip4, &self.enr_key.read());
                        match result {
                            Ok(_) => {
                                // Inform the connectivity state that we have updated our IP advertisement
                                self.connectivity_state.enr_socket_update(&new_ip4);
                                info!(ip_version="v4", %new_ip4, "Local UDP socket updated");
                                self.send_event(Event::SocketUpdated(new_ip4));
                            }
                            Err(e) => {
                                warn!(ip = %new_ip4, error = ?e, "Failed to update local UDP socket.");
                            }
                        }
                    }
                }
            }
            SocketAddr::V6(_) => {
                let local_ip6_socket = self.local_enr.read().udp6_socket();
                if let Some(ip_votes) = self.ip_votes.as_mut() {
                    ip_votes.insert(node_id, socket);
                    let maybe_ip6_majority = ip_votes.majority().1;

                    let new_ip6 = maybe_ip6_majority.and_then(|majority| {
                        if Some(majority) != local_ip6_socket { Some(majority) } else { None }
                    });
                    // Check if our advertised IPV6 address needs to be updated.
                    if let Some(new_ip6) = new_ip6 {
                        let new_ip6: SocketAddr = new_ip6.into();
                        let result =
                            self.local_enr.write().set_udp_socket(new_ip6, &self.enr_key.read());
                        match result {
                            Ok(_) => {
                                // Inform the connectivity state that we have updated our IP advertisement
                                self.connectivity_state.enr_socket_update(&new_ip6);
                                info!(ip_version="v6", %new_ip6, "Local UDP socket updated");
                                self.send_event(Event::SocketUpdated(new_ip6));
                            }
                            Err(e) => {
                                warn!(ip6 = %new_ip6, error = ?e, "Failed to update local UDP ip6 socket.");
                            }
                        }
                    }
                }
            }
        }
    }

    // Send RPC Requests //

    /// Sends a PING request to a node.
    fn send_ping(
        &mut self,
        enr: Enr,
        callback: Option<oneshot::Sender<Result<Pong, RequestError>>>,
    ) {
        match NodeContact::try_from_enr(enr, self.ip_mode) {
            Ok(contact) => {
                let request_body = RequestBody::Ping { enr_seq: self.local_enr.read().seq() };
                let active_request = ActiveRequest {
                    contact,
                    request_body,
                    query_id: None,
                    callback: callback.map(CallbackResponse::Pong),
                };
                self.send_rpc_request(active_request);
            }
            Err(NonContactable { enr }) => error!(%enr, "Trying to ping a non-contactable peer"),
        }
    }

    /// Ping all peers that are connected in the routing table.
    fn ping_connected_peers(&mut self) {
        // maintain the ping interval
        let connected_peers = {
            let mut kbuckets = self.kbuckets.write();
            kbuckets
                .iter()
                .filter_map(|entry| {
                    if entry.status.is_connected() { Some(entry.node.value.clone()) } else { None }
                })
                .collect::<Vec<_>>()
        };

        for enr in connected_peers {
            self.send_ping(enr.clone(), None);
        }
    }

    /// Request an external node's ENR.
    fn request_find_node_designated_peer(
        &mut self,
        contact: NodeContact,
        distances: Vec<u64>,
        callback: Option<oneshot::Sender<Result<Vec<Enr>, RequestError>>>,
    ) {
        let request_body = RequestBody::FindNode { distances };
        let active_request = ActiveRequest {
            contact,
            request_body,
            query_id: None,
            callback: callback.map(CallbackResponse::Nodes),
        };
        self.send_rpc_request(active_request);
    }

    /// Requests a TALK message from the peer.
    fn talk_request(
        &mut self,
        contact: NodeContact,
        protocol: Vec<u8>,
        request: Vec<u8>,
        callback: oneshot::Sender<Result<Vec<u8>, RequestError>>,
    ) {
        let request_body = RequestBody::Talk { protocol, request };

        let active_request = ActiveRequest {
            contact,
            request_body,
            query_id: None,
            callback: Some(CallbackResponse::Talk(callback)),
        };
        self.send_rpc_request(active_request);
    }

    /// Sends a NODES response, given a list of found ENR's. This function splits the nodes up
    /// into multiple responses to ensure the response stays below the maximum packet size.
    fn send_nodes_response(
        &mut self,
        node_address: NodeAddress,
        rpc_id: RequestId,
        mut distances: Vec<u64>,
    ) {
        let mut nodes_to_send = Vec::new();
        distances.sort_unstable();
        distances.dedup();

        if let Some(0) = distances.first() {
            // if the distance is 0 send our local ENR
            nodes_to_send.push(self.local_enr.read().clone());
            debug!("Sending our ENR to node: {}", node_address);
            distances.remove(0);
        }

        if !distances.is_empty() {
            let mut kbuckets = self.kbuckets.write();
            for node in kbuckets
                .nodes_by_distances(distances.as_slice(), self.config.max_nodes_response)
                .into_iter()
                .filter_map(|entry| {
                    if entry.node.key.preimage() != &node_address.node_id {
                        Some(entry.node.value.clone())
                    } else {
                        None
                    }
                })
            {
                nodes_to_send.push(node);
            }
        }

        // if there are no nodes, send an empty response
        if nodes_to_send.is_empty() {
            let response = Response {
                id: rpc_id,
                body: ResponseBody::Nodes { total: 1u64, nodes: Vec::new() },
            };
            trace!(
                to = %node_address.node_id,
                "Sending empty FINDNODES response",
            );
            if let Err(e) =
                self.handler_send.send(HandlerIn::Response(node_address, Box::new(response)))
            {
                warn!(error = %e, "Failed to send empty FINDNODES response")
            }
        } else {
            // build the NODES response
            let mut to_send_nodes: Vec<Vec<Enr>> = Vec::new();
            let mut total_size = 0;
            let mut rpc_index = 0;
            to_send_nodes.push(Vec::new());
            for enr in nodes_to_send.into_iter() {
                let entry_size = alloy_rlp::encode(&enr).len();
                // Responses assume that a session is established. Thus, on top of the encoded
                // ENR's the packet should be a regular message. A regular message has an IV (16
                // bytes), and a header of 55 bytes. The find-nodes RPC requires 16 bytes for the ID and the
                // `total` field. Also there is a 16 byte HMAC for encryption and an extra byte for
                // RLP encoding.
                //
                // We could also be responding via an authheader which can take up to 282 bytes in its
                // header.
                // As most messages will be normal messages we will try and pack as many ENR's we
                // can in and drop the response packet if a user requests an auth message of a very
                // packed response.
                //
                // The estimated total overhead for a regular message is therefore 104 bytes.
                if entry_size + total_size < MAX_PACKET_SIZE - 104 {
                    total_size += entry_size;
                    trace!(
                        %enr,
                        entry_size,
                        total_size,
                        "Adding ENR",
                    );
                    to_send_nodes[rpc_index].push(enr);
                } else {
                    total_size = entry_size;
                    to_send_nodes.push(vec![enr]);
                    rpc_index += 1;
                }
            }

            let responses: Vec<Response> = to_send_nodes
                .into_iter()
                .map(|nodes| Response {
                    id: rpc_id.clone(),
                    body: ResponseBody::Nodes { total: (rpc_index + 1) as u64, nodes },
                })
                .collect();

            for response in responses {
                trace!(
                    to = %node_address,
                    %response,
                    "Sending FINDNODES response",
                );
                if let Err(e) = self
                    .handler_send
                    .send(HandlerIn::Response(node_address.clone(), Box::new(response)))
                {
                    warn!(error = %e, "Failed to send FINDNODES response")
                }
            }
        }
    }

    /// Constructs and sends a request RPC to the session service given a `QueryInfo`.
    fn send_rpc_query(
        &mut self,
        query_id: QueryId,
        return_peer: NodeId,
        request_body: RequestBody,
    ) {
        // find the ENR associated with the query
        if let Some(enr) = self.find_enr(&return_peer) {
            match NodeContact::try_from_enr(enr, self.ip_mode) {
                Ok(contact) => {
                    let active_request = ActiveRequest {
                        contact,
                        request_body,
                        query_id: Some(query_id),
                        callback: None,
                    };
                    self.send_rpc_request(active_request);
                    // Request successfully sent
                    return;
                }
                Err(NonContactable { enr }) => {
                    // This can happen quite often in ipv6 only nodes
                    debug!("Query {} has a non contactable enr: {}", *query_id, enr);
                    debug!(query_id = *query_id, %enr, "Query has a non contactable enr");
                }
            }
        } else {
            error!(query_id = *query_id, "Query requested an unknown ENR");
        }

        // This query request has failed and we must inform the
        // query of the failed request.
        // TODO: Come up with a better design to ensure that all query RPC requests
        // are forced to be responded to.
        if let Some(query) = self.queries.get_mut(query_id) {
            query.on_failure(&return_peer);
        }
    }

    /// Sends generic RPC requests. Each request gets added to known outputs, awaiting a response.
    fn send_rpc_request(&mut self, active_request: ActiveRequest) {
        // Generate a random rpc_id which is matched per node id
        let id = RequestId::random();
        let request: Request =
            Request { id: id.clone(), body: active_request.request_body.clone() };
        let contact = active_request.contact.clone();

        debug!(%request, node = %contact,"Sending RPC to node");
        if self.handler_send.send(HandlerIn::Request(contact, Box::new(request))).is_ok() {
            self.active_requests.insert(id, active_request);
        }
    }

    fn send_event(&mut self, event: Event) {
        if let Some(stream) = self.event_stream.as_mut() {
            if let Err(mpsc::error::TrySendError::Closed(_)) = stream.try_send(event) {
                // If the stream has been dropped prevent future attempts to send events
                self.event_stream = None;
            }
        }
    }

    /// Processes discovered peers from a query.
    fn discovered(&mut self, source: &NodeId, mut enrs: Vec<Enr>, query_id: Option<QueryId>) {
        let local_id = self.local_enr.read().node_id();
        enrs.retain(|enr| {
            if enr.node_id() == local_id {
                return false;
            }

            // If there is an event stream send the Discovered event
            if self.config.report_discovered_peers {
                self.send_event(Event::Discovered(enr.clone()));
            }

            // Check that peers are compatible to be included into the routing table. They must:
            // - Pass the table filter
            // - Be contactable
            //
            // Failing this, they are not added, and if there is an older version of them in our
            // table, we remove them.
            let key = kbucket::Key::from(enr.node_id());
            if (self.config.table_filter)(enr) && self.ip_mode.get_contactable_addr(enr).is_some() {
                // If the ENR exists in the routing table and the discovered ENR has a greater
                // sequence number, perform some filter checks before updating the enr.

                let must_update_enr = match self.kbuckets.write().entry(&key) {
                    kbucket::Entry::Present(entry, _) => entry.value().seq() < enr.seq(),
                    kbucket::Entry::Pending(entry, _) => entry.value().seq() < enr.seq(),
                    _ => false,
                };

                if must_update_enr {
                    if let UpdateResult::Failed(reason) =
                        self.kbuckets.write().update_node(&key, enr.clone(), None)
                    {
                        self.peers_to_ping.remove(&enr.node_id());
                        debug!(node = %source, ?reason, "Failed to update discovered ENR.");

                        return false; // Remove this peer from the discovered list if the update failed
                    }
                }
            } else {
                // Is either non-contactable or didn't pass the table filter. If it exists in the
                // routing table, remove it.
                match self.kbuckets.write().entry(&key) {
                    kbucket::Entry::Present(entry, _) if entry.value().seq() < enr.seq() => {
                        entry.remove()
                    }
                    kbucket::Entry::Pending(entry, _) if entry.value().seq() < enr.seq() => {
                        entry.remove()
                    }
                    _ => {}
                }

                // Didn't pass the requirements remove the ENR
                return false;
            }

            // The remaining ENRs are used if this request was part of a query. If we are
            // requesting the target of the query, this ENR could be the result of requesting the
            // target-nodes own id. We don't want to add this as a "new" discovered peer in the
            // query, so we remove it from the discovered list here.
            source != &enr.node_id()
        });

        // if this is part of a query, update the query
        if let Some(query_id) = query_id {
            if let Some(query) = self.queries.get_mut(query_id) {
                let mut peer_count = 0;
                for enr_ref in enrs.iter() {
                    if !query
                        .target_mut()
                        .untrusted_enrs
                        .iter()
                        .any(|e| e.node_id() == enr_ref.node_id())
                    {
                        query.target_mut().untrusted_enrs.push(enr_ref.clone());
                    }
                    peer_count += 1;
                }
                debug!(peer_count, ?query_id, "peers found for query id");
                query.on_success(source, &enrs)
            } else {
                debug!(?query_id, "Response returned for ended query")
            }
        }
    }

    /// Update the connection status of a node in the routing table.
    /// This tracks whether or not we should be pinging peers. Disconnected peers are removed from
    /// the queue and newly added peers to the routing table are added to the queue.
    fn connection_updated(&mut self, node_id: NodeId, new_status: ConnectionStatus) {
        // Variables to that may require post-processing
        let mut ping_peer = None;
        let mut event_to_send = None;

        let key = kbucket::Key::from(node_id);
        match new_status {
            ConnectionStatus::Connected(enr, direction) => {
                // attempt to update or insert the new ENR.
                let status = NodeStatus { state: ConnectionState::Connected, direction };

                let insert_result =
                    self.kbuckets.write().insert_or_update(&key, enr.clone(), status);
                match insert_result {
                    InsertResult::Inserted => {
                        // We added this peer to the table
                        debug!(%node_id, "New connected node added to routing table");
                        self.peers_to_ping.insert(node_id);

                        // PING immediately if the direction is outgoing. This allows us to receive
                        // a PONG without waiting for the ping_interval, making ENR updates faster.
                        if direction == ConnectionDirection::Outgoing {
                            self.send_ping(enr, None);
                        }

                        let event = Event::NodeInserted { node_id, replaced: None };
                        event_to_send = Some(event);
                    }
                    InsertResult::Pending { disconnected } => {
                        ping_peer = Some(disconnected);
                    }
                    InsertResult::StatusUpdated { promoted_to_connected }
                    | InsertResult::Updated { promoted_to_connected } => {
                        // The node was updated
                        if promoted_to_connected {
                            debug!(%node_id, "Node promoted to connected");
                            self.peers_to_ping.insert(node_id);
                        }
                    }
                    InsertResult::ValueUpdated | InsertResult::UpdatedPending => {}
                    InsertResult::Failed(reason) => {
                        // On large networks with limited IPv6 nodes, it is hard to get enough
                        // PONG votes in order to estimate our external IP address. Often the
                        // routing table can be full, and so we reject useful IPv6 here.
                        //
                        // If we are low on votes and we initiated this connection (i.e it was not
                        // forced on us) then lets get a PONG from this node.

                        if direction == ConnectionDirection::Outgoing
                            && self.require_more_ip_votes(enr.udp6_socket().is_some())
                        {
                            self.send_ping(enr, None);
                        }

                        self.peers_to_ping.remove(&node_id);
                        trace!(%node_id, ?reason, "Could not insert node");
                    }
                }
            }
            ConnectionStatus::PongReceived => {
                match self.kbuckets.write().update_node_status(
                    &key,
                    ConnectionState::Connected,
                    None,
                ) {
                    UpdateResult::Failed(reason) => {
                        self.peers_to_ping.remove(&node_id);
                        debug!(node = %node_id, ?reason, "Could not update connection status from pong");
                    }
                    update => {
                        trace!(update_result = ?update, "Peer's conenction status has been updated based on the pong")
                    }
                }
            }
            ConnectionStatus::Disconnected => {
                // If the node has disconnected, remove any ping timer for the node.
                match self.kbuckets.write().update_node_status(
                    &key,
                    ConnectionState::Disconnected,
                    None,
                ) {
                    UpdateResult::Failed(reason) => match reason {
                        FailureReason::KeyNonExistent => {}
                        others => {
                            warn!(
                                node = %node_id,
                                reason = ?others,
                                "Could not update node to disconnected.",
                            );
                        }
                    },
                    _ => {
                        debug!(%node_id, "Node set to disconnected")
                    }
                }
                self.peers_to_ping.remove(&node_id);
            }
        };

        // Post processing

        if let Some(event) = event_to_send {
            self.send_event(event);
        }

        if let Some(node_key) = ping_peer {
            let optional_enr = {
                if let kbucket::Entry::Present(entry, _status) =
                    self.kbuckets.write().entry(&node_key)
                {
                    // NOTE: We don't check the status of this peer. We try and ping outdated peers.
                    Some(entry.value().clone())
                } else {
                    None
                }
            };
            if let Some(enr) = optional_enr {
                self.send_ping(enr, None)
            }
        }
    }

    /// The equivalent of libp2p `inject_connected()` for a udp session. We have no stream, but a
    /// session key-pair has been negotiated.
    fn inject_session_established(
        &mut self,
        enr: Enr,
        socket: &SocketAddr,
        connection_direction: ConnectionDirection,
    ) {
        // Inform the connectivity state that an incoming peer has connected to us. This could
        // establish that our externally advertised address is contactable.
        if matches!(connection_direction, ConnectionDirection::Incoming) {
            self.connectivity_state.received_incoming_connection(socket);
        }

        // Ignore sessions with non-contactable ENRs
        if self.ip_mode.get_contactable_addr(&enr).is_none() {
            return;
        }

        let node_id = enr.node_id();

        // We never update connection direction if a node already exists in the routing table as we
        // don't want to promote the direction from incoming to outgoing.
        let key = kbucket::Key::from(node_id);
        let direction = match self.kbuckets.read().get_bucket(&key).map(|bucket| bucket.get(&key)) {
            Some(Some(node)) => node.status.direction,
            _ => connection_direction,
        };

        debug!(node = %node_id, %direction, %socket, "Session established with Node");
        self.connection_updated(node_id, ConnectionStatus::Connected(enr, direction));
    }

    /// A session could not be established or an RPC request timed-out (after a few retries, if
    /// specified).
    fn rpc_failure(&mut self, id: RequestId, error: RequestError) {
        trace!(reason = ?error, %id, "RPC Error removing request.");
        if let Some(active_request) = self.active_requests.remove(&id) {
            // If this is initiated by the user, return an error on the callback. All callbacks
            // support a request error.
            match active_request.callback {
                Some(CallbackResponse::Nodes(callback)) => {
                    callback
                        .send(Err(error))
                        .unwrap_or_else(|_| debug!("Couldn't send Nodes error response to user"));
                    return;
                }
                Some(CallbackResponse::Talk(callback)) => {
                    // return the error
                    callback
                        .send(Err(error))
                        .unwrap_or_else(|_| debug!("Couldn't send TALK error response to user"));
                    return;
                }
                Some(CallbackResponse::Pong(callback)) => {
                    // return the error
                    callback
                        .send(Err(error))
                        .unwrap_or_else(|_| debug!("Couldn't send Pong error response to user"));
                    return;
                }
                None => {
                    // no callback to send too
                }
            }

            let node_id = active_request.contact.node_id();
            match active_request.request_body {
                // if a failed FindNodes request, ensure we haven't partially received packets. If
                // so, process the partially found nodes
                RequestBody::FindNode { ref distances } => {
                    if let Some(nodes_response) = self.active_nodes_responses.remove(&id) {
                        if !nodes_response.received_nodes.is_empty() {
                            let node_id = active_request.contact.node_id();
                            let addr = active_request.contact.socket_addr();
                            let received = nodes_response.received_nodes.len();
                            warn!(%node_id, %addr, %error, %received, requested_distances = ?distances, "FINDNODE request failed with partial results");
                            // if it's a query mark it as success, to process the partial
                            // collection of peers
                            self.discovered(
                                &node_id,
                                nodes_response.received_nodes,
                                active_request.query_id,
                            );
                        }
                    } else {
                        // there was no partially downloaded nodes inform the query of the failure
                        // if it's part of a query
                        if let Some(query_id) = active_request.query_id {
                            if let Some(query) = self.queries.get_mut(query_id) {
                                query.on_failure(&node_id);
                            }
                        } else {
                            debug!(
                                request_body = %active_request.request_body,
                                node = %active_request.contact,
                                "Failed RPC request",
                            );
                        }
                    }
                }
                // for all other requests, if any are queries, mark them as failures.
                _ => {
                    if let Some(query_id) = active_request.query_id {
                        if let Some(query) = self.queries.get_mut(query_id) {
                            debug!(
                                request_body = %active_request.request_body,
                                query_id = *query_id,
                                node = %active_request.contact,
                                "Failed query request",
                            );
                            query.on_failure(&node_id);
                        }
                    } else {
                        debug!(
                            request_body = %active_request.request_body,
                            node = %active_request.contact,
                            error = ?error,
                            "Failed RPC request",
                        );
                    }
                }
            }

            self.connection_updated(node_id, ConnectionStatus::Disconnected);
        }
    }

    /// Helper function that determines if we need more votes for a specific IP
    /// class.
    ///
    /// If we are in dual-stack mode and don't have enough votes for either ipv4 or ipv6 and the
    /// requesting node/vote is what we need, then this will return true.
    fn require_more_ip_votes(&mut self, is_ipv6: bool) -> bool {
        if !matches!(self.ip_mode, IpMode::DualStack) {
            return false;
        }

        let Some(ip_votes) = self.ip_votes.as_mut() else {
            return false;
        };

        // Here we check the number of non-expired votes, rather than the majority. As if the
        // local router is not SNAT'd we can have many votes but none for the same port and we
        // therefore do excessive pinging.
        match (ip_votes.has_minimum_threshold(), is_ipv6) {
                    // We don't have enough ipv4 votes, and this is an IPv4-only node.
                    ((false, true), false) |
                    // We don't have enough ipv6 votes, and this is an IPv6 node.
                    ((true, false), true) |
                    // We don't have enough ipv6 or ipv4 nodes, permit this peer's pong.
                    ((false, false), _,) => true,
                    // We have enough votes do nothing.
                    ((_, _), _,) =>  false,
            }
    }

    /// A future that maintains the routing table and inserts nodes when required. This returns the
    /// [`Event::NodeInserted`] variant if a new node has been inserted into the routing table.
    async fn bucket_maintenance_poll(kbuckets: &Arc<RwLock<KBucketsTable<NodeId, Enr>>>) -> Event {
        future::poll_fn(move |_cx| {
            // Drain applied pending entries from the routing table.
            if let Some(entry) = kbuckets.write().take_applied_pending() {
                let event = Event::NodeInserted {
                    node_id: entry.inserted.into_preimage(),
                    replaced: entry.evicted.map(|n| n.key.into_preimage()),
                };
                return Poll::Ready(event);
            }
            Poll::Pending
        })
        .await
    }

    /// A future the maintains active queries. This returns completed and timed out queries, as
    /// well as queries which need to be driven further with extra requests.
    async fn query_event_poll(queries: &mut QueryPool<QueryInfo, NodeId, Enr>) -> QueryEvent {
        future::poll_fn(move |_cx| match queries.poll() {
            QueryPoolState::Finished(query) => Poll::Ready(QueryEvent::Finished(Box::new(query))),
            QueryPoolState::Waiting(Some((query, return_peer))) => {
                let node_id = return_peer;
                let request_body = query.target().rpc_request(return_peer);
                Poll::Ready(QueryEvent::Waiting(query.id(), node_id, request_body))
            }

            QueryPoolState::Timeout(query) => Poll::Ready(QueryEvent::TimedOut(Box::new(query))),
            QueryPoolState::Waiting(None) | QueryPoolState::Idle => Poll::Pending,
        })
        .await
    }
}

/// The result of the `query_event_poll` indicating an action is required to further progress an
/// active query.
enum QueryEvent {
    /// The query is waiting for a peer to be contacted.
    Waiting(QueryId, NodeId, RequestBody),
    /// The query has timed out, possible returning peers.
    TimedOut(Box<crate::query_pool::Query<QueryInfo, NodeId, Enr>>),
    /// The query has completed successfully.
    Finished(Box<crate::query_pool::Query<QueryInfo, NodeId, Enr>>),
}

/// The types of queries that can be made.
pub enum QueryKind {
    /// A FindNode query. Searches for peers that are closest to a particular target.
    FindNode { target_node: NodeId },
    /// A predicate query. Searches for peers that are close to a target but filtered by a specific
    /// predicate and limited by a target peer count.
    Predicate {
        target_node: NodeId,
        target_peer_no: usize,
        predicate: Box<dyn Fn(&Enr) -> bool + Send>,
    },
}

impl core::fmt::Debug for QueryKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FindNode { target_node } => {
                f.debug_struct("FindNode").field("target_node", target_node).finish()
            }
            Self::Predicate { target_node, target_peer_no, .. } => f
                .debug_struct("Predicate")
                .field("target_node", target_node)
                .field("target_peer_no", target_peer_no)
                .finish_non_exhaustive(),
        }
    }
}
/// Reporting the connection status of a node.
enum ConnectionStatus {
    /// A node has started a new connection with us.
    Connected(Enr, ConnectionDirection),
    /// We received a Pong from a new node. Do not have the connection direction.
    PongReceived,
    /// The node has disconnected
    Disconnected,
}
