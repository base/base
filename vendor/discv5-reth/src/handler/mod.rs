//! Session and packet handling for the Discv5 Discovery service.
//!
//! The [`Handler`] is responsible for establishing and maintaining sessions with
//! connected/discovered nodes. Each node, identified by it's [`NodeId`] is associated with a
//! `Session`. This service drives the handshakes for establishing the sessions and associated
//! logic for sending/requesting initial connections/ENR's to/from unknown peers.
//!
//! The [`Handler`] also manages the timeouts for each request and reports back RPC failures,
//! and received messages. Messages are encrypted and decrypted using the
//! associated `Session` for each node.
//!
//! An ongoing established connection is abstractly represented by a `Session`. A node that provides an ENR with an
//! IP address/port that doesn't match the source, is considered invalid. A node that doesn't know
//! their external contactable addresses should set their ENR IP field to `None`.
//!
//! The Handler also routinely checks the timeouts for banned nodes and removes them from the
//! banned list once their ban expires.
//!
//! # Usage
//!
//! Interacting with a handler is done via channels. A Handler is spawned using the [`Handler::spawn`]
//! function. This returns an exit channel, a sending and receiving channel respectively. If the
//! exit channel is dropped or fired, the handler task gets shutdown.
//!
//! Requests from the application layer can be made via the receive channel using a [`HandlerIn`].
//! Responses from the application layer can be made via the receive channel using a [`HandlerIn`].
//! Messages from a node on the network come by [`Socket`] and get the form of a [`HandlerOut`]
//! and can be forwarded to the application layer via the send channel.
use std::{
    collections::HashMap,
    convert::TryFrom,
    default::Default,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use delay_map::HashMapDelay;
use enr::{CombinedKey, NodeId};
use futures::prelude::*;
use more_asserts::debug_unreachable;
use parking_lot::RwLock;
use smallvec::SmallVec;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace, warn};

use crate::{
    Enr, ProtocolIdentity,
    config::Config,
    discv5::PERMIT_BAN_LIST,
    error::{Error, RequestError},
    packet::{ChallengeData, IdNonce, MessageNonce, Packet, PacketKind},
    rpc::{Message, Request, RequestBody, RequestId, Response, ResponseBody},
    socket,
    socket::{FilterConfig, Socket, UnrecognizedFrame},
};

mod active_requests;
mod crypto;
mod request_call;
mod session;
mod tests;

use active_requests::ActiveRequests;
use request_call::RequestCall;
use session::Session;

pub use crate::node_info::{NodeAddress, NodeContact};
use crate::{lru_time_cache::LruTimeCache, metrics::METRICS, socket::ListenConfig};

// The time interval to check banned peer timeouts and unban peers when the timeout has elapsed (in
// seconds).
const BANNED_NODES_CHECK: u64 = 300; // Check every 5 minutes.

/// Messages sent from the application layer to `Handler`.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum HandlerIn {
    /// A Request to send to a `NodeContact` has been received from the application layer. A
    /// `NodeContact` is an abstract type that allows for either an ENR to be sent or a `Raw` type
    /// which represents an `SocketAddr`, `PublicKey` and `NodeId`. This type can be created from
    /// MultiAddrs and MultiAddr strings for some keys.
    ///
    /// This permits us to send messages to nodes without knowing their ENR. In this case their ENR
    /// will be requested during the handshake.
    ///
    /// A Request is flagged and permits responses through the packet filter.
    ///
    /// Note: To update an ENR for an unknown node, we request a FINDNODE with distance 0 to the
    /// `NodeContact` we know of.
    Request(NodeContact, Box<Request>),

    /// A Response to send to a particular node to answer a HandlerOut::Request has been
    /// received from the application layer.
    ///
    /// The handler does not keep state of requests, so the application layer must send the
    /// response back to the `NodeAddress` from which the request was received.
    Response(NodeAddress, Box<Response>),

    /// A Random packet has been received and we have requested the application layer to inform
    /// us what the highest known ENR is for this node.
    /// The `WhoAreYouRef` is sent out in the `HandlerOut::WhoAreYou` event and should
    /// be returned here to submit the application's response.
    WhoAreYou(WhoAreYouRef, Option<Enr>),
}

/// Messages sent between a node on the network and `Handler`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOut {
    /// A session has been established with a node.
    ///
    /// A session is only considered established once we have received a signed ENR from the
    /// node and either the observed `SocketAddr` matches the one declared in the ENR or the
    /// ENR declares no `SocketAddr`.
    Established(Enr, SocketAddr, ConnectionDirection),

    /// A Request has been received from a node on the network.
    Request(NodeAddress, Box<Request>),

    /// A Response has been received from a node on the network.
    Response(NodeAddress, Box<Response>),

    /// An unknown source has requested information from us. Return the reference with the known
    /// ENR of this node (if known). See the `HandlerIn::WhoAreYou` variant.
    WhoAreYou(WhoAreYouRef),

    /// An RPC request failed.
    ///
    /// This returns the request ID and an error indicating why the request failed.
    RequestFailed(RequestId, RequestError),

    /// A peer advertising an ENR that doesn't verify against its observed socket and node ID.
    ///
    /// These peers are denied sessions.
    UnverifiableEnr { enr: Enr, socket: SocketAddr, node_id: NodeId },
    /// A frame that could not be decoded as discv5.
    UnrecognizedFrame(UnrecognizedFrame),
    /// These sessions have expired from the cache.
    ExpiredSessions(Vec<NodeAddress>),
}

/// How we connected to the node.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum ConnectionDirection {
    /// The node contacted us.
    Incoming,
    /// We contacted the node.
    Outgoing,
}

/// A reference for the application layer to send back when the handler requests any known
/// ENR for the NodeContact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoAreYouRef(pub NodeAddress, MessageNonce);

#[derive(Debug)]
/// A Challenge (WHOAREYOU) object used to handle and send WHOAREYOU requests.
pub struct Challenge {
    /// The challenge data received from the node.
    data: ChallengeData,
    /// The remote's ENR if we know it. We can receive a challenge from an unknown node.
    remote_enr: Option<Enr>,
}

/// Request ID from the handler's perspective.
#[derive(Debug, Clone)]
enum HandlerReqId {
    /// Requests made by the handler.
    Internal(RequestId),
    /// Requests made from outside the handler.
    External(RequestId),
}

/// A request queued for sending.
struct PendingRequest {
    contact: NodeContact,
    request_id: HandlerReqId,
    request: RequestBody,
}

impl From<&HandlerReqId> for RequestId {
    fn from(id: &HandlerReqId) -> Self {
        match id {
            HandlerReqId::Internal(id) => id.clone(),
            HandlerReqId::External(id) => id.clone(),
        }
    }
}

/// Process to handle handshakes and sessions established from raw RPC communications between nodes.
pub struct Handler {
    /// Configuration for the discv5 service.
    request_retries: u8,
    /// The local node id to save unnecessary read locks on the ENR. The NodeID should not change
    /// during the operation of the server.
    node_id: NodeId,
    /// The protocol id used
    protocol_identity: ProtocolIdentity,
    /// The local ENR.
    enr: Arc<RwLock<Enr>>,
    /// The key to sign the ENR and set up encrypted communication with peers.
    key: Arc<RwLock<CombinedKey>>,
    /// Active requests that are awaiting a response.
    active_requests: ActiveRequests,
    /// The expected responses by SocketAddr which allows packets to pass the underlying filter.
    filter_expected_responses: Arc<RwLock<HashMap<SocketAddr, usize>>>,
    /// Requests awaiting a handshake completion.
    pending_requests: HashMap<NodeAddress, Vec<PendingRequest>>,
    /// Currently in-progress outbound handshakes (WHOAREYOU packets) with peers.
    active_challenges: HashMapDelay<NodeAddress, Challenge>,
    /// Established sessions with peers.
    sessions: LruTimeCache<NodeAddress, Session>,
    /// The channel to receive messages from the application layer.
    service_recv: mpsc::UnboundedReceiver<HandlerIn>,
    /// The channel to send messages to the application layer.
    service_send: mpsc::Sender<HandlerOut>,
    /// The listening sockets to filter out any attempted requests to self.
    listen_sockets: SmallVec<[SocketAddr; 2]>,
    /// The discovery v5 UDP socket tasks.
    socket: Socket,
    /// Exit channel to shutdown the handler.
    exit: oneshot::Receiver<()>,
}

type HandlerReturn =
    (oneshot::Sender<()>, mpsc::UnboundedSender<HandlerIn>, mpsc::Receiver<HandlerOut>);

impl Handler {
    /// A new Session service which instantiates the UDP socket send/recv tasks.
    pub async fn spawn(
        enr: Arc<RwLock<Enr>>,
        key: Arc<RwLock<CombinedKey>>,
        config: Config,
    ) -> Result<HandlerReturn, std::io::Error> {
        let (exit_sender, exit) = oneshot::channel();
        // create the channels to send/receive messages from the application
        let (handler_send, service_recv) = mpsc::unbounded_channel();
        let (service_send, handler_recv) = mpsc::channel(50);

        // Creates a SocketConfig to pass to the underlying UDP socket tasks.

        // Lets the underlying filter know that we are expecting a packet from this source.
        let filter_expected_responses = Arc::new(RwLock::new(HashMap::new()));

        // The local node id
        let node_id = enr.read().node_id();

        // enable the packet filter if required
        let filter_config = FilterConfig {
            enabled: config.enable_packet_filter,
            rate_limiter: config.filter_rate_limiter.clone(),
            max_nodes_per_ip: config.filter_max_nodes_per_ip,
            max_bans_per_ip: config.filter_max_bans_per_ip,
        };

        let mut listen_sockets = SmallVec::default();
        match config.listen_config {
            ListenConfig::Ipv4 { ip, port } => listen_sockets.push((ip, port).into()),
            ListenConfig::Ipv6 { ip, port } => listen_sockets.push((ip, port).into()),
            ListenConfig::DualStack { ipv4, ipv4_port, ipv6, ipv6_port } => {
                listen_sockets.push((ipv4, ipv4_port).into());
                listen_sockets.push((ipv6, ipv6_port).into());
            }
            ListenConfig::FromSockets { ref ipv4, ref ipv6 } => {
                if let Some(s) = ipv4 {
                    listen_sockets.push(s.local_addr().expect("socket must have local addr"));
                }
                if let Some(s) = ipv6 {
                    listen_sockets.push(s.local_addr().expect("socket must have local addr"));
                }
            }
        };

        let socket_config = socket::SocketConfig {
            executor: config.executor.clone().expect("Executor must exist"),
            filter_config,
            listen_config: config.listen_config.clone(),
            local_node_id: node_id,
            protocol_identity: config.protocol_identity,
            expected_responses: filter_expected_responses.clone(),
            ban_duration: config.ban_duration,
        };

        // Attempt to bind to the socket before spinning up the send/recv tasks.
        let socket = Socket::new(socket_config).await?;

        config.executor.clone().expect("Executor must be present").spawn(Box::pin(async move {
            let mut handler = Handler {
                request_retries: config.request_retries,
                node_id,
                protocol_identity: config.protocol_identity,
                enr,
                key,
                active_requests: ActiveRequests::new(config.request_timeout),
                pending_requests: HashMap::new(),
                filter_expected_responses,
                sessions: LruTimeCache::new(
                    config.session_timeout,
                    Some(config.session_cache_capacity),
                ),
                active_challenges: HashMapDelay::new(config.request_timeout),
                service_recv,
                service_send,
                listen_sockets,
                socket,
                exit,
            };
            debug!("Handler Starting");
            handler.start().await;
        }));

        Ok((exit_sender, handler_send, handler_recv))
    }

    /// The main execution loop for the handler.
    async fn start(&mut self) {
        let mut banned_nodes_check = tokio::time::interval(Duration::from_secs(BANNED_NODES_CHECK));

        loop {
            tokio::select! {
                Some(handler_request) = self.service_recv.recv() => {
                    match handler_request {
                        HandlerIn::Request(contact, request) => {
                            let Request { id, body: request } = *request;
                            if let Err(request_error) =  self.send_request(contact, HandlerReqId::External(id.clone()), request).await {
                                // If the sending failed report to the application
                                if let Err(e) = self.service_send.send(HandlerOut::RequestFailed(id, request_error)).await {
                                    warn!(error = %e, "Failed to inform that request failed")
                                }
                            }
                        }
                        HandlerIn::Response(dst, response) => self.send_response(dst, *response).await,
                        HandlerIn::WhoAreYou(wru_ref, enr) => self.send_challenge(wru_ref, enr).await,
                    }
                }
                Some(incoming_packet) = self.socket.recv.recv() => {
                    match incoming_packet {
                        socket::RecvPacket::Inbound(inbound_packet) => {
                            self.process_inbound_packet(inbound_packet).await;
                        }
                        socket::RecvPacket::UnrecognizedFrame(frame) => {
                            if let Err(e) = self
                                .service_send
                                .send(HandlerOut::UnrecognizedFrame(frame))
                                .await
                            {
                                warn!(error = %e, "Failed to inform of unrecognized frame")
                            }
                        }
                    }
                }
                Some(Ok((node_address, active_request))) = self.active_requests.next() => {
                    self.handle_request_timeout(node_address, active_request).await;
                }
                Some(Ok((node_address, _challenge))) = self.active_challenges.next() => {
                    // A challenge has expired. There could be pending requests awaiting this
                    // challenge. We process them here
                    self.send_pending_requests(&node_address).await;
                }
                _ = banned_nodes_check.tick() => self.unban_nodes_check(), // Unban nodes that are past the timeout
                _ = &mut self.exit => {
                    return;
                }
            }
        }
    }

    /// Processes an inbound decoded packet.
    async fn process_inbound_packet(&mut self, inbound_packet: socket::InboundPacket) {
        let message_nonce = inbound_packet.header.message_nonce;
        match inbound_packet.header.kind {
            PacketKind::WhoAreYou { enr_seq, .. } => {
                let challenge_data =
                    ChallengeData::try_from(inbound_packet.authenticated_data.as_slice())
                        .expect("Must be correct size");
                self.handle_challenge(
                    inbound_packet.src_address,
                    message_nonce,
                    enr_seq,
                    challenge_data,
                )
                .await
            }
            PacketKind::Handshake { src_id, id_nonce_sig, ephem_pubkey, enr_record } => {
                let node_address =
                    NodeAddress { socket_addr: inbound_packet.src_address, node_id: src_id };
                self.handle_auth_message(
                    node_address,
                    message_nonce,
                    &id_nonce_sig,
                    &ephem_pubkey,
                    enr_record,
                    &inbound_packet.message,
                    &inbound_packet.authenticated_data, // This is required for authenticated data in decryption.
                )
                .await
            }
            PacketKind::Message { src_id } => {
                let node_address =
                    NodeAddress { socket_addr: inbound_packet.src_address, node_id: src_id };
                self.handle_message(
                    node_address,
                    message_nonce,
                    &inbound_packet.message,
                    &inbound_packet.authenticated_data,
                )
                .await
            }
        }
    }

    fn remove_expected_response(&mut self, socket_addr: SocketAddr) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.filter_expected_responses.write().entry(socket_addr)
        {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if count == &0 {
                entry.remove();
            }
        }
    }

    fn add_expected_response(&mut self, socket_addr: SocketAddr) {
        *self.filter_expected_responses.write().entry(socket_addr).or_default() += 1;
    }

    /// A request has timed out.
    async fn handle_request_timeout(
        &mut self,
        node_address: NodeAddress,
        mut request_call: RequestCall,
    ) {
        if request_call.retries() >= self.request_retries {
            trace!(%node_address, "Request timed out");
            // Remove the request from the awaiting packet_filter
            self.remove_expected_response(node_address.socket_addr);
            // The request has timed out. We keep any established session for future use.
            self.fail_request(request_call, RequestError::Timeout, false).await;
        } else {
            // increment the request retry count and restart the timeout
            trace!(
                body = %request_call.body(),
                %node_address,
                "Resending message",
            );
            self.send(node_address.clone(), request_call.packet().clone()).await;
            request_call.increment_retries();
            self.active_requests.insert(node_address, request_call);
        }
    }

    /// Sends a `Request` to a node.
    async fn send_request(
        &mut self,
        contact: NodeContact,
        request_id: HandlerReqId,
        request: RequestBody,
    ) -> Result<(), RequestError> {
        let node_address = contact.node_address();

        if self.listen_sockets.contains(&node_address.socket_addr) {
            debug!("Filtered request to self");
            return Err(RequestError::SelfRequest);
        }

        // If there is already an active challenge (WHOAREYOU sent) for this node, or if we are
        // awaiting a session with this node to be established, add the request to pending requests.
        if self.active_challenges.get(&node_address).is_some()
            || self.is_awaiting_session_to_be_established(&node_address)
        {
            trace!(%node_address, "Request queued for node");
            self.pending_requests.entry(node_address).or_default().push(PendingRequest {
                contact,
                request_id,
                request,
            });
            return Ok(());
        }

        let (packet, initiating_session) = {
            if let Some(session) = self.sessions.get_mut(&node_address) {
                // Encrypt the message and send
                let request = match &request_id {
                    HandlerReqId::Internal(id) | HandlerReqId::External(id) => {
                        Request { id: id.clone(), body: request.clone() }
                    }
                };
                let packet = session
                    .encrypt_message(self.node_id, &request.encode(), self.protocol_identity)
                    .map_err(|e| RequestError::EncryptionFailed(format!("{e:?}")))?;
                (packet, false)
            } else {
                // No session exists, start a new handshake initiating a new session
                trace!(
                    %node_address,
                    "Starting session. Sending random packet",
                );
                let packet = Packet::new_random(&self.node_id, self.protocol_identity)
                    .map_err(RequestError::EntropyFailure)?;
                (packet, true)
            }
        };

        let call =
            RequestCall::new(contact, packet.clone(), request_id, request, initiating_session);
        // let the filter know we are expecting a response
        self.add_expected_response(node_address.socket_addr);
        self.send(node_address.clone(), packet).await;

        self.active_requests.insert(node_address, call);
        Ok(())
    }

    /// Sends an RPC Response.
    async fn send_response(&mut self, node_address: NodeAddress, response: Response) {
        // Check for an established session
        let packet = if let Some(session) = self.sessions.get_mut(&node_address) {
            session.encrypt_message(self.node_id, &response.encode(), self.protocol_identity)
        } else {
            // Either the session is being established or has expired. We simply drop the
            // response in this case.
            return warn!(
                %response,
                node = %node_address.node_id,
                "Session is not established. Dropping response",
            );
        };

        match packet {
            Ok(packet) => self.send(node_address, packet).await,
            Err(e) => warn!(error = ?e, "Could not encrypt response"),
        }
    }

    /// This is called in response to a `HandlerOut::WhoAreYou` event. The applications finds the
    /// highest known ENR for a node then we respond to the node with a WHOAREYOU packet.
    async fn send_challenge(&mut self, wru_ref: WhoAreYouRef, remote_enr: Option<Enr>) {
        let node_address = wru_ref.0;
        let message_nonce = wru_ref.1;

        if self.active_challenges.get(&node_address).is_some() {
            warn!(%node_address, "WHOAREYOU already sent.");
            return;
        }

        // NOTE: We do not check if we have an active session here. This was checked before
        // requesting the ENR from the service. It could be the case we have established a session
        // in the meantime, we allow this challenge to establish a second session in the event this
        // race occurs. The nodes will decide amongst themselves which session keys to use (the
        // most recent).

        // It could be the case we have sent an ENR with an active request, however we consider
        // these independent as this is in response to an unknown packet. If the ENR it not in our
        // table (remote_enr is None) then we re-request the ENR to keep the session up to date.

        // send the challenge
        let enr_seq = remote_enr.clone().map_or_else(|| 0, |enr| enr.seq());
        let id_nonce: IdNonce = rand_08::random();
        let packet =
            Packet::new_whoareyou(message_nonce, id_nonce, self.protocol_identity, enr_seq);
        let challenge_data = ChallengeData::try_from(packet.authenticated_data().as_slice())
            .expect("Must be the correct challenge size");
        debug!(%node_address, "Sending WHOAREYOU");
        self.add_expected_response(node_address.socket_addr);
        self.send(node_address.clone(), packet).await;
        self.active_challenges.insert(node_address, Challenge { data: challenge_data, remote_enr });
    }

    /* Packet Handling */

    /// Handles a WHOAREYOU packet that was received from the network.
    async fn handle_challenge(
        &mut self,
        src_address: SocketAddr,
        request_nonce: MessageNonce,
        enr_seq: u64,
        challenge_data: ChallengeData,
    ) {
        // Check that this challenge matches a known active request.
        // If this message passes all the requisite checks, a request call is returned.
        let mut request_call = match self.active_requests.remove_by_nonce(&request_nonce) {
            Some((node_address, request_call)) => {
                // Verify that the src_addresses match
                if node_address.socket_addr != src_address {
                    debug!(
                        source = %src_address,
                        expected_source = %node_address.socket_addr,
                        message_nonce = hex::encode(request_nonce),
                        "Received a WHOAREYOU packet for a message with a non-expected source.",
                    );
                    // Add the request back if src_address doesn't match
                    self.active_requests.insert(node_address, request_call);
                    return;
                }
                request_call
            }
            None => {
                trace!(
                    source = %src_address,
                    message_nonce = hex::encode(request_nonce),
                    "Received a WHOAREYOU packet that references an unknown or expired request."
                );
                return;
            }
        };

        // double check the message nonces match
        if request_call.packet().message_nonce() != &request_nonce {
            // This could theoretically happen if a peer uses the same node id across
            // different connections.
            warn!(
                source = %request_call.contact(),
                message_nonce = hex::encode(request_call.packet().message_nonce()),
                expected_nonce = hex::encode(request_nonce),
                "Received a WHOAREYOU from a non expected source."
            );
            // NOTE: Both mappings are removed in this case.
            return;
        }

        trace!(
            source = %request_call.contact(),
            "Received a WHOAREYOU packet response.",
        );

        // We do not allow multiple WHOAREYOU packets for a single challenge request. If we have
        // already sent a WHOAREYOU ourselves, we drop sessions who send us a WHOAREYOU in
        // response.
        if request_call.handshake_sent() {
            warn!(
                node = %request_call.contact(),
                "Authentication response already sent. Dropping session.",
            );
            self.fail_request(request_call, RequestError::InvalidRemotePacket, true).await;
            return;
        }

        // Encrypt the message with an auth header and respond

        // First if a new version of our ENR is requested, obtain it for the header
        let updated_enr =
            if enr_seq < self.enr.read().seq() { Some(self.enr.read().clone()) } else { None };

        // Generate a new session and authentication packet
        let (auth_packet, mut session) = match Session::encrypt_with_header(
            request_call.contact(),
            self.key.clone(),
            updated_enr,
            &self.node_id,
            self.protocol_identity,
            &challenge_data,
            &request_call.encode(),
        ) {
            Ok(v) => v,
            Err(e) => {
                error!(error = ?e, "Could not generate a session");
                self.fail_request(request_call, RequestError::InvalidRemotePacket, true).await;
                return;
            }
        };

        // There are two quirks with an established session at this point.
        // 1. We may not know the ENR. In this case we need to set up a request to find the ENR and
        //    wait for a response before we officially call this node established.
        // 2. The challenge here could be to an already established session. If so, we need to
        //    update the existing session to attempt to decrypt future messages with the new keys
        //    and update the keys internally upon successful decryption.
        //
        // We handle both of these cases here.

        // Check if we know the ENR, if not request it and flag the session as awaiting an ENR.
        //
        // All sent requests must have an associated node_id. Therefore the following
        // must not panic.
        let node_address = request_call.contact().node_address();
        let auth_message_nonce = auth_packet.header.message_nonce;
        match request_call.contact().enr() {
            Some(enr) => {
                // NOTE: Here we decide if the session is outgoing or ingoing. The condition for an
                // outgoing session is that we originally sent a RANDOM packet (signifying we did
                // not have a session for a request) and the packet is not a PING (we are not
                // trying to update an old session that may have expired.
                let connection_direction = if request_call.initiating_session() {
                    ConnectionDirection::Outgoing
                } else {
                    ConnectionDirection::Incoming
                };

                // We already know the ENR. Send the handshake response packet
                trace!(
                    %node_address,
                    request_call.id = ?request_call.id(),
                    "Sending Authentication response to node",
                );
                request_call.update_packet(auth_packet.clone());
                request_call.set_handshake_sent();
                request_call.set_initiating_session(false);
                // Reinsert the request_call
                self.insert_active_request(request_call);
                // Send the actual packet to the send task.
                self.send(node_address.clone(), auth_packet).await;

                // Notify the application that the session has been established
                self.service_send
                    .send(HandlerOut::Established(
                        enr,
                        node_address.socket_addr,
                        connection_direction,
                    ))
                    .await
                    .unwrap_or_else(|e| warn!(error = %e, "Error with sending channel"));
            }
            None => {
                // Don't know the ENR. Establish the session, but request an ENR also

                // Send the Auth response
                let contact = request_call.contact().clone();
                trace!(
                    %node_address,
                    request_call.id = ?request_call.id(),
                    "Sending Authentication response to node",
                );
                request_call.update_packet(auth_packet.clone());
                request_call.set_handshake_sent();
                // Reinsert the request_call
                self.insert_active_request(request_call);
                self.send(node_address.clone(), auth_packet).await;

                let id = RequestId::random();
                let request = RequestBody::FindNode { distances: vec![0] };
                session.awaiting_enr = Some(id.clone());
                if let Err(e) =
                    self.send_request(contact, HandlerReqId::Internal(id), request).await
                {
                    warn!(error = %e, "Failed to send Enr request")
                }
            }
        }
        self.new_session(node_address.clone(), session, Some(auth_message_nonce)).await;
    }

    /// Verifies a Node ENR to it's observed address. If it fails, any associated session is also
    /// considered failed. If it succeeds, we notify the application.
    fn verify_enr(&self, enr: &Enr, node_address: &NodeAddress) -> bool {
        // If the ENR does not match the observed IP addresses, we consider the Session
        // failed.
        enr.node_id() == node_address.node_id
            && match node_address.socket_addr {
                SocketAddr::V4(socket_addr) => {
                    enr.udp4_socket().is_none_or(|advertized_addr| socket_addr == advertized_addr)
                }
                SocketAddr::V6(socket_addr) => {
                    enr.udp6_socket().is_none_or(|advertized_addr| socket_addr == advertized_addr)
                }
            }
    }

    async fn notify_unverifiable_enr(&self, enr: Enr, socket: SocketAddr, node_id: NodeId) {
        self.service_send
            .send(HandlerOut::UnverifiableEnr { enr, socket, node_id })
            .await
            .unwrap_or_else(|e| warn!(error = %e, "Error with sending channel"))
    }

    /// Handle a message that contains an authentication header.
    #[allow(clippy::too_many_arguments)]
    async fn handle_auth_message(
        &mut self,
        node_address: NodeAddress,
        message_nonce: MessageNonce,
        id_nonce_sig: &[u8],
        ephem_pubkey: &[u8],
        enr_record: Option<Enr>,
        message: &[u8],
        authenticated_data: &[u8],
    ) {
        // Needs to match an outgoing challenge packet (so we have the required nonce to be signed). If it doesn't we drop the packet.
        // This will lead to future outgoing challenges if they proceed to send further encrypted
        // packets.
        trace!(
            from = %node_address,
            "Received an Authentication header message",
        );

        if let Some(challenge) = self.active_challenges.remove(&node_address) {
            match Session::establish_from_challenge(
                self.key.clone(),
                &self.node_id,
                &node_address.node_id,
                challenge,
                id_nonce_sig,
                ephem_pubkey,
                enr_record,
            ) {
                Ok((session, enr)) => {
                    // Remove the expected response for the challenge.
                    self.remove_expected_response(node_address.socket_addr);
                    // Receiving an AuthResponse must give us an up-to-date view of the node ENR.
                    // Verify the ENR is valid
                    if self.verify_enr(&enr, &node_address) {
                        // Session is valid
                        // Notify the application
                        // The session established here are from WHOAREYOU packets that we sent.
                        // This occurs when a node established a connection with us.
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::Established(
                                enr,
                                node_address.socket_addr,
                                ConnectionDirection::Incoming,
                            ))
                            .await
                        {
                            warn!(error = %e, "Failed to inform of established session")
                        }
                    } else {
                        // IP's or NodeAddress don't match.
                        //
                        // We still handle the request, but we do not add the ENR to our routing
                        // table or consider the ENR valid.
                        debug!(
                            udp4_socket = ?enr.udp4_socket(),
                            udp6_socket = ?enr.udp6_socket(),
                            expected = %node_address,
                            "Session has invalid ENR",
                        );

                        // The ENR doesn't verify. Notify application.
                        self.notify_unverifiable_enr(
                            enr,
                            node_address.socket_addr,
                            node_address.node_id,
                        )
                        .await;
                    }

                    // When (re-)establishing a session from an outgoing challenge, we do not need
                    // to filter out this request from active requests, so we do not pass
                    // the message nonce on to `new_session`.
                    self.new_session(node_address.clone(), session, None).await;
                    self.handle_message(
                        node_address.clone(),
                        message_nonce,
                        message,
                        authenticated_data,
                    )
                    .await;
                }
                Err(Error::InvalidChallengeSignature(challenge)) => {
                    warn!(
                        %node_address,
                        "Authentication header contained invalid signature. Ignoring packet from node",
                    );
                    // insert back the challenge
                    self.active_challenges.insert(node_address, *challenge);
                }
                Err(e) => {
                    warn!(
                        error = ?e,
                        "Invalid Authentication header. Dropping session",
                    );
                    self.fail_session(&node_address, RequestError::InvalidRemotePacket, true).await;
                }
            }
        } else {
            warn!(
                node_id = %node_address.node_id, addr = %node_address.socket_addr,
                "Received an authenticated header without a matching WHOAREYOU request",
            );
        }
    }

    /// Send all pending requests corresponding to the given node address, that were waiting for a
    /// new session to be established or when an active outgoing challenge has expired.
    async fn send_pending_requests(&mut self, node_address: &NodeAddress) {
        let pending_requests = self.pending_requests.remove(node_address).unwrap_or_default();
        for req in pending_requests {
            trace!(
                request_id = %RequestId::from(&req.request_id),
                %node_address,
                request = %req.request,
                "Sending pending request",
            );
            if let Err(request_error) =
                self.send_request(req.contact, req.request_id.clone(), req.request).await
            {
                warn!(error = %request_error, "Failed to send next pending request");
                // Inform the service that the request failed
                match req.request_id {
                    HandlerReqId::Internal(_) => {
                        // An internal request could not be sent. For now we do nothing about
                        // this.
                    }
                    HandlerReqId::External(id) => {
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::RequestFailed(id, request_error))
                            .await
                        {
                            warn!(error = %e, "Failed to inform that request failed");
                        }
                    }
                }
            }
        }
    }

    /// Replays all active requests for the given node address, in the case that a new session has
    /// been established. If an optional message nonce is provided, the corresponding request will
    /// be skipped, eg. the request that established the new session.
    async fn replay_active_requests(
        &mut self,
        node_address: &NodeAddress,
        // Optional message nonce to filter out the request used to establish the session.
        message_nonce: Option<MessageNonce>,
    ) {
        trace!(
            %node_address,
            ?message_nonce,
            "Replaying active requests",
        );

        let packets = if let Some(session) = self.sessions.get_mut(node_address) {
            let mut packets = vec![];
            for request_call in
                self.active_requests.get(node_address).unwrap_or(&vec![]).iter().filter(|req| {
                    // Except the active request that was used to establish the new session, as it has
                    // already been handled and shouldn't be replayed.
                    if let Some(nonce) = message_nonce.as_ref() {
                        req.packet().message_nonce() != nonce
                    } else {
                        true
                    }
                })
            {
                if let Ok(new_packet) = session.encrypt_message(
                    self.node_id,
                    &request_call.encode(),
                    self.protocol_identity,
                ) {
                    packets.push((*request_call.packet().message_nonce(), new_packet));
                } else {
                    error!(
                        id = ?request_call.id(),
                        "Failed to re-encrypt packet while replaying active request with id",
                    );
                }
            }

            packets
        } else {
            debug_unreachable!("Attempted to replay active requests but session doesn't exist.");
            error!("Attempted to replay active requests but session doesn't exist.");
            return;
        };

        for (old_nonce, new_packet) in packets {
            self.active_requests.update_packet(old_nonce, new_packet.clone());
            self.send(node_address.clone(), new_packet).await;
        }
    }

    /// Handle a standard message that does not contain an authentication header.
    #[allow(clippy::single_match)]
    async fn handle_message(
        &mut self,
        node_address: NodeAddress,
        message_nonce: MessageNonce,
        message: &[u8],
        authenticated_data: &[u8],
    ) {
        // check if we have an available session
        if let Some(session) = self.sessions.get_mut(&node_address) {
            // attempt to decrypt and process the message.
            let message = match session.decrypt_message(message_nonce, message, authenticated_data)
            {
                Ok(m) => match Message::decode(&m) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = ?e, %node_address, "Failed to decode message");
                        return;
                    }
                },
                Err(e) => {
                    // We have a session, but the message could not be decrypted. It is likely the node
                    // sending this message has dropped their session. In this case, this message is a
                    // Random packet and we should reply with a WHOAREYOU.
                    // This means we need to drop the current session and re-establish.
                    trace!(error = %e, "Decryption failed");
                    debug!(
                        %node_address,
                        "Message from node is not encrypted with known session keys.",
                    );
                    self.fail_session(&node_address, RequestError::InvalidRemotePacket, true).await;
                    // If we haven't already sent a WhoAreYou,
                    // spawn a WHOAREYOU event to check for highest known ENR
                    if self.active_challenges.get(&node_address).is_none() {
                        let whoareyou_ref = WhoAreYouRef(node_address, message_nonce);
                        if let Err(e) =
                            self.service_send.send(HandlerOut::WhoAreYou(whoareyou_ref)).await
                        {
                            warn!(error = %e, "Failed to send WhoAreYou to the service")
                        }
                    } else {
                        trace!(%node_address, "WHOAREYOU packet already sent");
                    }
                    return;
                }
            };

            trace!(%node_address, "Received message");

            // Remove any associated request from pending_request
            match message {
                Message::Request(request) => {
                    // report the request to the application
                    if let Err(e) = self
                        .service_send
                        .send(HandlerOut::Request(node_address, Box::new(request)))
                        .await
                    {
                        warn!(error = %e, "Failed to report request to application")
                    }
                }
                Message::Response(response) => {
                    // Sessions could be awaiting an ENR response. Check if this response matches
                    // these
                    if let Some(request_id) = session.awaiting_enr.as_ref() {
                        if &response.id == request_id {
                            session.awaiting_enr = None;
                            match response.body {
                                ResponseBody::Nodes { mut nodes, .. } => {
                                    // Received the requested ENR
                                    if let Some(enr) = nodes.pop() {
                                        if self.verify_enr(&enr, &node_address) {
                                            // Notify the application
                                            // This can occur when we try to dial a node without an
                                            // ENR. In this case we have attempted to establish the
                                            // connection, so this is an outgoing connection.
                                            if let Err(e) = self
                                                .service_send
                                                .send(HandlerOut::Established(
                                                    enr,
                                                    node_address.socket_addr,
                                                    ConnectionDirection::Outgoing,
                                                ))
                                                .await
                                            {
                                                warn!(error = %e, "Failed to inform established outgoing connection")
                                            }
                                            return;
                                        }

                                        // The ENR doesn't verify. Notify application.
                                        self.notify_unverifiable_enr(
                                            enr,
                                            node_address.socket_addr,
                                            node_address.node_id,
                                        )
                                        .await;
                                    }
                                }
                                _ => {}
                            }

                            debug!("Session failed invalid ENR response");
                            self.fail_session(&node_address, RequestError::InvalidRemoteEnr, true)
                                .await;
                            return;
                        }
                    }
                    // Handle standard responses
                    self.handle_response(node_address, response).await;
                }
            }
        } else {
            // no session exists
            trace!(%node_address, "Received a message without a session.");
            trace!("Requesting a WHOAREYOU packet to be sent.");
            // spawn a WHOAREYOU event to check for highest known ENR
            let whoareyou_ref = WhoAreYouRef(node_address, message_nonce);
            if let Err(e) = self.service_send.send(HandlerOut::WhoAreYou(whoareyou_ref)).await {
                warn!(
                    error = %e,
                    "Spawn a WHOAREYOU event to check for highest known ENR failed",
                )
            }
        }
    }

    /// Handles a response to a request. Re-inserts the request call if the response is a multiple
    /// Nodes response.
    async fn handle_response(&mut self, node_address: NodeAddress, response: Response) {
        // Find a matching request, if any
        if let Some(mut request_call) =
            self.active_requests.remove_request(&node_address, &response.id)
        {
            // The response matches a request
            // Check to see if this is a Nodes response, in which case we may require to wait for
            // extra responses
            if let ResponseBody::Nodes { total, .. } = response.body {
                if total > 1 {
                    // This is a multi-response Nodes response
                    if let Some(remaining_responses) = request_call.remaining_responses_mut() {
                        *remaining_responses -= 1;
                        if remaining_responses != &0 {
                            // more responses remaining, add back the request and send the response
                            // add back the request and send the response
                            self.active_requests.insert(node_address.clone(), request_call);
                            if let Err(e) = self
                                .service_send
                                .send(HandlerOut::Response(node_address, Box::new(response)))
                                .await
                            {
                                warn!(error = %e, "Failed to inform of response")
                            }
                            return;
                        }
                    } else {
                        // This is the first instance
                        *request_call.remaining_responses_mut() = Some(total - 1);
                        // add back the request and send the response
                        self.active_requests.insert(node_address.clone(), request_call);
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::Response(node_address, Box::new(response)))
                            .await
                        {
                            warn!(error = %e, "Failed to inform of response")
                        }
                        return;
                    }
                }
            }

            // Remove the expected response
            self.remove_expected_response(node_address.socket_addr);

            // The request matches report the response
            if let Err(e) = self
                .service_send
                .send(HandlerOut::Response(node_address.clone(), Box::new(response)))
                .await
            {
                warn!(error = %e, "Failed to inform of response")
            }
        } else {
            // This is likely a late response and we have already failed the request. These get
            // dropped here.
            trace!(%node_address, "Late response from node");
        }
    }

    /// Inserts a request and associated auth_tag mapping.
    fn insert_active_request(&mut self, request_call: RequestCall) {
        let node_address = request_call.contact().node_address();

        // adds the mapping of message nonce to node address
        self.active_requests.insert(node_address, request_call);
    }

    /// Establishes a new session with a peer, or re-establishes an existing session if a
    /// new challenge was issued during an ongoing session.
    async fn new_session(
        &mut self,
        node_address: NodeAddress,
        session: Session,
        // Optional message nonce is required to filter out the request that was used in the
        // handshake to re-establish a session, if applicable.
        message_nonce: Option<MessageNonce>,
    ) {
        // Clear the session cache and report expired elements
        self.remove_expired_sessions().await;

        if let Some(current_session) = self.sessions.get_mut(&node_address) {
            current_session.update(session);
            // If a session is re-established, due to a new handshake during an ongoing
            // session, we need to replay any active requests from the prior session, excluding
            // the request that was used to re-establish the session handshake.
            self.replay_active_requests(&node_address, message_nonce).await;
        } else {
            self.sessions.insert(node_address.clone(), session);
            METRICS.active_sessions.store(self.sessions.len(), Ordering::Relaxed);
            // We could have pending messages that were awaiting this session to be
            // established. If so process them.
            self.send_pending_requests(&node_address).await;
        }
    }

    /// A request has failed.
    async fn fail_request(
        &mut self,
        request_call: RequestCall,
        error: RequestError,
        remove_session: bool,
    ) {
        // The Request has expired, remove the session.
        // Fail the current request
        match request_call.id() {
            HandlerReqId::Internal(_) => {
                // Do not report failures on requests belonging to the handler.
            }
            HandlerReqId::External(id) => {
                if let Err(e) = self
                    .service_send
                    .send(HandlerOut::RequestFailed(id.clone(), error.clone()))
                    .await
                {
                    warn!(error = %e, "Failed to inform request failure")
                }
            }
        }

        let node_address = request_call.contact().node_address();
        self.fail_session(&node_address, error, remove_session).await;
    }

    /// Removes a session, fails all of that session's active & pending requests, and updates associated metrics and fields.
    async fn fail_session(
        &mut self,
        node_address: &NodeAddress,
        error: RequestError,
        remove_session: bool,
    ) {
        if remove_session {
            // Remove expired sessions
            self.remove_expired_sessions().await;

            self.sessions.remove(node_address);
            METRICS.active_sessions.store(self.sessions.len(), Ordering::Relaxed);
        }
        // fail all pending requests
        if let Some(to_remove) = self.pending_requests.remove(node_address) {
            for PendingRequest { request_id, .. } in to_remove {
                match request_id {
                    HandlerReqId::Internal(_) => {
                        // Do not report failures on requests belonging to the handler.
                    }
                    HandlerReqId::External(id) => {
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::RequestFailed(id, error.clone()))
                            .await
                        {
                            warn!(error = %e, "Failed to inform request failure")
                        }
                    }
                }
            }
        }
        // fail all active requests
        for req in self.active_requests.remove_requests(node_address).unwrap_or_default() {
            match req.id() {
                HandlerReqId::Internal(_) => {
                    // Do not report failures on requests belonging to the handler.
                }
                HandlerReqId::External(id) => {
                    if let Err(e) = self
                        .service_send
                        .send(HandlerOut::RequestFailed(id.clone(), error.clone()))
                        .await
                    {
                        warn!(error = %e, "Failed to inform request failure")
                    }
                }
            }
            self.remove_expected_response(node_address.socket_addr);
        }
    }

    /// Sends a packet to the send handler to be encoded and sent.
    async fn send(&mut self, node_address: NodeAddress, packet: Packet) {
        let outbound_packet = socket::OutboundPacket { node_address, packet };
        if let Err(e) = self.socket.send.send(outbound_packet).await {
            warn!(error = %e, "Failed to send outbound packet")
        }
    }

    /// Check if any banned nodes have served their time and unban them.
    fn unban_nodes_check(&self) {
        PERMIT_BAN_LIST
            .write()
            .ban_ips
            .retain(|_, time| time.is_none() || Some(Instant::now()) < *time);
        PERMIT_BAN_LIST
            .write()
            .ban_nodes
            .retain(|_, time| time.is_none() || Some(Instant::now()) < *time);
    }

    /// Removes expired sessions and report them back to the service.
    async fn remove_expired_sessions(&mut self) {
        // Purge any expired sessions
        let expired_sessions = self.sessions.remove_expired_values();

        if !expired_sessions.is_empty() {
            if let Err(e) =
                self.service_send.send(HandlerOut::ExpiredSessions(expired_sessions)).await
            {
                warn!(error = %e, "Failed to inform app of expired sessions")
            }
        }
    }

    /// Returns whether a session with this node does not exist and a request that initiates
    /// a session has been sent.
    fn is_awaiting_session_to_be_established(&mut self, node_address: &NodeAddress) -> bool {
        if self.sessions.get(node_address).is_some() {
            // session exists
            return false;
        }

        if let Some(requests) = self.active_requests.get(node_address) {
            requests.iter().any(|req| req.initiating_session())
        } else {
            false
        }
    }
}
