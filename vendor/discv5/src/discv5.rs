//! The Discovery v5 protocol. See the module level docs for further details.
//!
//! This provides the main struct for running and interfacing with a discovery v5 server.
//!
//! A [`Discv5`] struct needs to be created either with an [`crate::executor::Executor`] specified
//! in the [`Config`] via the [`crate::ConfigBuilder`] or in the presence of a tokio runtime that
//! has timing and io enabled.
//!
//! Once a [`Discv5`] struct has been created the service is started by running the [`Discv5::start`]
//! functions with a UDP socket. This will start a discv5 server in the background listening on the
//! specified UDP socket.
//!
//! The server can be shutdown using the [`Discv5::shutdown`] function.

use std::{
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use enr::{CombinedKey, EnrKey, Error as EnrError, NodeId};
#[cfg(feature = "libp2p")]
use multiaddr::Multiaddr;
use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::{
    Config, Enr, IpMode,
    error::{Error, QueryError, RequestError},
    kbucket::{
        self, ConnectionDirection, ConnectionState, FailureReason, InsertResult, KBucketsTable,
        NodeStatus, UpdateResult,
    },
    node_info::{NodeAddress, NodeContact},
    service::{QueryKind, Service, ServiceRequest, TalkRequest},
};
// Create lazy static variable for the global permit/ban list
use crate::{
    metrics::{METRICS, Metrics},
    service::Pong,
};

lazy_static! {
    pub static ref PERMIT_BAN_LIST: RwLock<crate::PermitBanList> =
        RwLock::new(crate::PermitBanList::default());
}

pub(crate) mod test;

/// Events that can be produced by the `Discv5` event stream.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// A node has been discovered from a FINDNODES request.
    ///
    /// The ENR of the node is returned. Various properties can be derived from the ENR.
    /// This happen spontaneously through queries as nodes return ENR's. These ENR's are not
    /// guaranteed to be live or contactable.
    Discovered(Enr),
    /// A new node has been added to the routing table.
    NodeInserted { node_id: NodeId, replaced: Option<NodeId> },
    /// An ENR doesn't verify against the observed socket and node ID of the peer.
    UnverifiableEnr { enr: Enr, socket: SocketAddr, node_id: NodeId },
    /// A new session has been established with a node.
    SessionEstablished(Enr, SocketAddr),
    /// A session has been removed from our cache due to inactivity.
    SessionsExpired(Vec<NodeAddress>),
    /// Our local ENR IP address has been updated.
    SocketUpdated(SocketAddr),
    /// A node has initiated a talk request.
    TalkRequest(TalkRequest),
}

/// The main Discv5 Service struct. This provides the user-level API for performing queries and
/// interacting with the underlying service.
pub struct Discv5 {
    config: Config,
    /// The channel to make requests from the main service.
    service_channel: Option<mpsc::Sender<ServiceRequest>>,
    /// The exit channel to shutdown the underlying service.
    service_exit: Option<oneshot::Sender<()>>,
    /// The routing table of the discv5 service.
    kbuckets: Arc<RwLock<KBucketsTable<NodeId, Enr>>>,
    /// The local ENR of the server.
    local_enr: Arc<RwLock<Enr>>,
    /// The key associated with the local ENR, required for updating the local ENR.
    enr_key: Arc<RwLock<CombinedKey>>,
    // Type of socket we are using
    ip_mode: IpMode,
}

impl Discv5 {
    pub fn new(
        local_enr: Enr,
        enr_key: CombinedKey,
        mut config: Config,
    ) -> Result<Self, &'static str> {
        // ensure the keypair matches the one that signed the enr.
        if local_enr.public_key() != enr_key.public() {
            return Err("Provided keypair does not match the provided ENR");
        }

        // If an executor is not provided, assume a current tokio runtime is running. If not panic.
        if config.executor.is_none() {
            config.executor = Some(Box::<crate::executor::TokioExecutor>::default());
        };

        // NOTE: Currently we don't expose custom filter support in the configuration. Users can
        // optionally use the IP filter via the ip_limit configuration parameter. In the future, we
        // may expose this functionality to the users if there is demand for it.
        let (table_filter, bucket_filter) = if config.ip_limit {
            (
                Some(Box::new(kbucket::IpTableFilter) as Box<dyn kbucket::Filter<Enr>>),
                Some(Box::new(kbucket::IpBucketFilter) as Box<dyn kbucket::Filter<Enr>>),
            )
        } else {
            (None, None)
        };

        let local_enr = Arc::new(RwLock::new(local_enr));
        let enr_key = Arc::new(RwLock::new(enr_key));
        let kbuckets = Arc::new(RwLock::new(KBucketsTable::new(
            local_enr.read().node_id().into(),
            Duration::from_secs(60),
            config.incoming_bucket_limit,
            table_filter,
            bucket_filter,
        )));

        // Update the PermitBan list based on initial configuration
        *PERMIT_BAN_LIST.write() = config.permit_ban_list.clone();

        let ip_mode = IpMode::new_from_listen_config(&config.listen_config);

        Ok(Discv5 {
            config,
            service_channel: None,
            service_exit: None,
            kbuckets,
            local_enr,
            enr_key,
            ip_mode,
        })
    }

    /// Starts the required tasks and begins listening on a given UDP SocketAddr.
    pub async fn start(&mut self) -> Result<(), Error> {
        if self.service_channel.is_some() {
            warn!("Service is already started");
            return Err(Error::ServiceAlreadyStarted);
        }

        // create the main service
        let (service_exit, service_channel) = Service::spawn(
            self.local_enr.clone(),
            self.enr_key.clone(),
            self.kbuckets.clone(),
            self.config.clone(),
        )
        .await?;
        self.service_exit = Some(service_exit);
        self.service_channel = Some(service_channel);
        Ok(())
    }

    /// Terminates the service.
    pub fn shutdown(&mut self) {
        if let Some(exit) = self.service_exit.take() {
            if exit.send(()).is_err() {
                debug!("Discv5 service already shutdown");
            }
            self.service_channel = None;
        } else {
            debug!("Service is already shutdown");
        }
    }

    /// Adds a known ENR of a peer participating in Service to the
    /// routing table.
    ///
    /// This allows pre-populating the Kademlia routing table with known
    /// addresses, so that they can be used immediately in following DHT
    /// operations involving one of these peers, without having to dial
    /// them upfront.
    pub fn add_enr(&self, enr: Enr) -> Result<(), &'static str> {
        // only add ENR's that have a valid udp socket.
        if self.ip_mode.get_contactable_addr(&enr).is_none() {
            warn!(
                "ENR attempted to be added without an UDP socket compatible with configured IpMode has been ignored."
            );
            return Err("ENR has no compatible UDP socket to connect to");
        }

        if !(self.config.table_filter)(&enr) {
            warn!("ENR attempted to be added which is banned by the configuration table filter.");
            return Err("ENR banned by table filter");
        }

        let key = kbucket::Key::from(enr.node_id());

        match self.kbuckets.write().insert_or_update(
            &key,
            enr,
            NodeStatus {
                state: ConnectionState::Disconnected,
                direction: ConnectionDirection::Incoming,
            },
        ) {
            InsertResult::Inserted
            | InsertResult::Pending { .. }
            | InsertResult::StatusUpdated { .. }
            | InsertResult::ValueUpdated
            | InsertResult::Updated { .. }
            | InsertResult::UpdatedPending => Ok(()),
            InsertResult::Failed(FailureReason::BucketFull) => Err("Table full"),
            InsertResult::Failed(FailureReason::BucketFilter) => Err("Failed bucket filter"),
            InsertResult::Failed(FailureReason::TableFilter) => Err("Failed table filter"),
            InsertResult::Failed(FailureReason::InvalidSelfUpdate) => Err("Invalid self update"),
            InsertResult::Failed(_) => Err("Failed to insert ENR"),
        }
    }

    /// Removes a `node_id` from the routing table.
    ///
    /// This allows applications, for whatever reason, to remove nodes from the local routing
    /// table. Returns `true` if the node was in the table and `false` otherwise.
    pub fn remove_node(&self, node_id: &NodeId) -> bool {
        let key = &kbucket::Key::from(*node_id);
        self.kbuckets.write().remove(key)
    }

    /// Returns a vector of closest nodes by the given distances.
    pub fn nodes_by_distance(&self, mut distances: Vec<u64>) -> Vec<Enr> {
        let mut nodes_to_send = Vec::new();
        distances.sort_unstable();
        distances.dedup();

        if let Some(0) = distances.first() {
            // if the distance is 0 send our local ENR
            nodes_to_send.push(self.local_enr.read().clone());
            distances.remove(0);
        }

        if !distances.is_empty() {
            let mut kbuckets = self.kbuckets.write();
            for node in kbuckets
                .nodes_by_distances(distances.as_slice(), self.config.max_nodes_response)
                .into_iter()
                .map(|entry| entry.node.value.clone())
            {
                nodes_to_send.push(node);
            }
        }
        nodes_to_send
    }

    pub fn ip_mode(&self) -> IpMode {
        self.ip_mode
    }

    /// Mark a node in the routing table as `Disconnected`.
    ///
    /// A `Disconnected` node will be present in the routing table and will be only
    /// used if there are no other `Connected` peers in the bucket.
    /// Returns `true` if node was in table and `false` otherwise.
    pub fn disconnect_node(&self, node_id: &NodeId) -> bool {
        let key = &kbucket::Key::from(*node_id);
        !matches!(
            self.kbuckets.write().update_node_status(key, ConnectionState::Disconnected, None),
            UpdateResult::Failed(_)
        )
    }

    /// Returns the number of connected peers that exist in the routing table.
    pub fn connected_peers(&self) -> usize {
        self.kbuckets.write().iter().filter(|entry| entry.status.is_connected()).count()
    }

    /// Gets the metrics associated with the Server
    pub fn metrics(&self) -> Metrics {
        Metrics::from(&METRICS)
    }

    /// Exposes the raw reference to the underlying internal metrics.
    pub fn raw_metrics() -> &'static METRICS {
        &METRICS
    }

    /// Returns the local ENR of the node.
    pub fn local_enr(&self) -> Enr {
        self.local_enr.read().clone()
    }

    /// Identical to `Discv5::local_enr` except that this exposes the `Arc` itself.
    ///
    /// This is useful for synchronising views of the local ENR outside of `Discv5`.
    pub fn external_enr(&self) -> Arc<RwLock<Enr>> {
        self.local_enr.clone()
    }

    /// Returns the routing table of the discv5 service
    pub fn kbuckets(&self) -> KBucketsTable<NodeId, Enr> {
        self.kbuckets.read().clone()
    }

    /// Returns an ENR if one is known for the given NodeId.
    pub fn find_enr(&self, node_id: &NodeId) -> Option<Enr> {
        // check if we know this node id in our routing table
        let key = kbucket::Key::from(*node_id);
        if let kbucket::Entry::Present(entry, _) = self.kbuckets.write().entry(&key) {
            return Some(entry.value().clone());
        }
        None
    }

    /// Sends a PING request to a node.
    pub fn send_ping(
        &self,
        enr: Enr,
    ) -> impl Future<Output = Result<Pong, RequestError>> + 'static {
        let (callback_send, callback_recv) = oneshot::channel();
        let channel = self.clone_channel();

        async move {
            let channel = channel.map_err(|_| RequestError::ServiceNotStarted)?;

            let event = ServiceRequest::Ping(enr, Some(callback_send));

            // send the request
            channel
                .send(event)
                .await
                .map_err(|_| RequestError::ChannelFailed("Service channel closed".into()))?;
            // await the response
            callback_recv.await.map_err(|e| RequestError::ChannelFailed(e.to_string()))?
        }
    }

    /// Bans a node from the server. This will remove the node from the routing table if it exists
    /// and block all incoming packets from the node until the timeout specified. Setting the
    /// timeout to `None` creates a permanent ban.
    pub fn ban_node(&self, node_id: &NodeId, duration_of_ban: Option<Duration>) {
        let time_to_unban = duration_of_ban.map(|v| Instant::now() + v);
        self.remove_node(node_id);
        PERMIT_BAN_LIST.write().ban_nodes.insert(*node_id, time_to_unban);
    }

    /// Removes a banned node from the banned list.
    pub fn ban_node_remove(&self, node_id: &NodeId) {
        PERMIT_BAN_LIST.write().ban_nodes.remove(node_id);
    }

    /// Permits a node, allowing the node to bypass the packet filter.
    pub fn permit_node(&self, node_id: &NodeId) {
        PERMIT_BAN_LIST.write().permit_nodes.insert(*node_id);
    }

    /// Removes a node from the permit list.
    pub fn permit_node_remove(&self, node_id: &NodeId) {
        PERMIT_BAN_LIST.write().permit_nodes.remove(node_id);
    }

    /// Bans an IP from the server.  This will block all incoming packets from the IP.
    pub fn ban_ip(&self, ip: std::net::IpAddr, duration_of_ban: Option<Duration>) {
        let time_to_unban = duration_of_ban.map(|v| Instant::now() + v);
        PERMIT_BAN_LIST.write().ban_ips.insert(ip, time_to_unban);
    }

    /// Removes a banned IP from the banned list.
    pub fn ban_ip_remove(&self, ip: &std::net::IpAddr) {
        PERMIT_BAN_LIST.write().ban_ips.remove(ip);
    }

    /// Permits an IP, allowing the all packets from the IP to bypass the packet filter.
    pub fn permit_ip(&self, ip: std::net::IpAddr) {
        PERMIT_BAN_LIST.write().permit_ips.insert(ip);
    }

    /// Removes an IP from the permit list.
    pub fn permit_ip_remove(&self, ip: &std::net::IpAddr) {
        PERMIT_BAN_LIST.write().permit_ips.remove(ip);
    }

    /// Updates the local ENR TCP/UDP socket.
    pub fn update_local_enr_socket(&self, socket_addr: SocketAddr, is_tcp: bool) -> bool {
        let mut local_enr = self.local_enr.write();
        match (is_tcp, socket_addr) {
            (false, SocketAddr::V4(specific_socket_addr)) => {
                if Some(specific_socket_addr) != local_enr.udp4_socket() {
                    return local_enr.set_udp_socket(socket_addr, &self.enr_key.read()).is_ok();
                }
            }
            (true, SocketAddr::V4(specific_socket_addr)) => {
                if Some(specific_socket_addr) != local_enr.tcp4_socket() {
                    return local_enr.set_tcp_socket(socket_addr, &self.enr_key.read()).is_ok();
                }
            }
            (false, SocketAddr::V6(specific_socket_addr)) => {
                if Some(specific_socket_addr) != local_enr.udp6_socket() {
                    return local_enr.set_udp_socket(socket_addr, &self.enr_key.read()).is_ok();
                }
            }
            (true, SocketAddr::V6(specific_socket_addr)) => {
                if Some(specific_socket_addr) != local_enr.tcp6_socket() {
                    return local_enr.set_tcp_socket(socket_addr, &self.enr_key.read()).is_ok();
                }
            }
        }
        false
    }

    /// Allows application layer to insert an arbitrary field into the local ENR.
    pub fn enr_insert<T: alloy_rlp::Encodable>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<Option<Vec<u8>>, EnrError> {
        self.local_enr
            .write()
            .insert(key, value, &self.enr_key.read())
            .map(|v| v.map(|v| v.to_vec()))
    }

    /// Returns an iterator over all ENR node IDs of nodes currently contained in the routing table.
    pub fn table_entries_id(&self) -> Vec<NodeId> {
        self.kbuckets.write().iter().map(|entry| *entry.node.key.preimage()).collect()
    }

    /// Returns an iterator over all the ENR's of nodes currently contained in the routing table.
    pub fn table_entries_enr(&self) -> Vec<Enr> {
        self.kbuckets.write().iter().map(|entry| entry.node.value.clone()).collect()
    }

    /// Returns an iterator over all the entries in the routing table.
    pub fn table_entries(&self) -> Vec<(NodeId, Enr, NodeStatus)> {
        self.kbuckets
            .write()
            .iter()
            .map(|entry| (*entry.node.key.preimage(), entry.node.value.clone(), entry.status))
            .collect()
    }

    /// Takes a closure parameterized by type `Arc<RwLock<KBucketsTable<NodeId, Enr>>>` as
    /// parameter. Caution: caller is responsible of dropping a lock taken on the kbuckets. For
    /// example, a read lock can be taken on the kbuckets to optimistically view the current keys
    /// in the kbuckets (optimistic since it doesn't apply pending entries, which requires a write
    /// lock).
    /// ```
    /// use std::str::FromStr;
    /// use discv5::{ConfigBuilder, Discv5, ListenConfig, Enr, enr::CombinedKey};
    ///
    /// let sk = CombinedKey::generate_secp256k1();
    /// let enr = Enr::builder().build(&sk).unwrap();
    /// let config = ConfigBuilder::new(ListenConfig::default()).build();
    /// let discv5: Discv5 = Discv5::new(enr, sk, config).unwrap();
    ///
    /// let entries =  discv5.with_kbuckets(|kbuckets| kbuckets
    ///     .read()
    ///     .iter_ref()
    ///     .map(|entry| *entry.node.key.preimage())
    ///     .collect::<Vec<_>>());
    /// ```
    pub fn with_kbuckets<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&Arc<RwLock<KBucketsTable<NodeId, Enr>>>) -> T,
    {
        f(&self.kbuckets)
    }

    /// Requests the ENR of a node corresponding to multiaddr or multi-addr string.
    ///
    /// Only `ed25519` and `secp256k1` key types are currently supported.
    ///
    /// Note: The async syntax is forgone here in order to create `'static` futures, where the
    /// underlying sending channel is cloned.
    #[cfg(feature = "libp2p")]
    #[cfg_attr(docsrs, doc(cfg(feature = "libp2p")))]
    pub fn request_enr(
        &self,
        multiaddr: impl std::convert::TryInto<Multiaddr> + 'static,
    ) -> impl Future<Output = Result<Enr, RequestError>> + 'static {
        let channel = self.clone_channel();

        async move {
            let channel = channel.map_err(|_| RequestError::ServiceNotStarted)?;
            // Sanitize the multiaddr

            // The multiaddr must support the udp protocol and be of an appropriate key type.
            // The conversion logic is contained in the `TryFrom<MultiAddr>` implementation of a
            // `NodeContact`.
            let multiaddr: Multiaddr = multiaddr
                .try_into()
                .map_err(|_| RequestError::InvalidMultiaddr("Could not convert to multiaddr"))?;
            let node_contact: NodeContact = NodeContact::try_from_multiaddr(multiaddr)
                .map_err(RequestError::InvalidMultiaddr)?;

            let (callback_send, callback_recv) = oneshot::channel();

            let event =
                ServiceRequest::FindNodeDesignated(node_contact.clone(), vec![0], callback_send);

            // send the request
            channel
                .send(event)
                .await
                .map_err(|_| RequestError::ChannelFailed("Service channel closed".into()))?;
            // await the response
            match callback_recv.await.map_err(|e| RequestError::ChannelFailed(e.to_string()))? {
                Ok(mut nodes) => {
                    // This must be for asking for an ENR
                    if nodes.len() > 1 {
                        warn!("Peer returned more than one ENR for itself. {}", node_contact);
                    }
                    nodes.pop().ok_or(RequestError::InvalidEnr("Peer did not return an ENR"))
                }
                Err(err) => Err(err),
            }
        }
    }

    /// Request a TALK message from a node, identified via the NodeContact.
    pub fn talk_req(
        &self,
        node_contact: NodeContact,
        protocol: Vec<u8>,
        request: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, RequestError>> + 'static {
        // convert the ENR to a node_contact.

        let (callback_send, callback_recv) = oneshot::channel();
        let channel = self.clone_channel();

        async move {
            let channel = channel.map_err(|_| RequestError::ServiceNotStarted)?;

            let event = ServiceRequest::Talk(node_contact, protocol, request, callback_send);

            // send the request
            channel
                .send(event)
                .await
                .map_err(|_| RequestError::ChannelFailed("Service channel closed".into()))?;
            // await the response
            callback_recv.await.map_err(|e| RequestError::ChannelFailed(e.to_string()))?
        }
    }

    /// Send a FINDNODE request for nodes that fall within the given set of distances,
    /// to the designated peer and wait for a response.
    pub fn find_node_designated_peer(
        &self,
        enr: Enr,
        distances: Vec<u64>,
    ) -> impl Future<Output = Result<Vec<Enr>, RequestError>> + 'static {
        let (callback_send, callback_recv) = oneshot::channel();
        let channel = self.clone_channel();
        let ip_mode = self.ip_mode;

        async move {
            let node_contact = NodeContact::try_from_enr(enr, ip_mode)?;
            let channel = channel.map_err(|_| RequestError::ServiceNotStarted)?;

            let event = ServiceRequest::FindNodeDesignated(node_contact, distances, callback_send);

            // send the request
            channel
                .send(event)
                .await
                .map_err(|_| RequestError::ChannelFailed("Service channel closed".into()))?;
            // await the response
            callback_recv.await.map_err(|e| RequestError::ChannelFailed(e.to_string()))?
        }
    }

    /// Runs an iterative `FIND_NODE` request.
    ///
    /// This will return peers containing contactable nodes of the DHT closest to the
    /// requested `NodeId`.
    ///
    /// Note: The async syntax is forgone here in order to create `'static` futures, where the
    /// underlying sending channel is cloned.
    pub fn find_node(
        &self,
        target_node: NodeId,
    ) -> impl Future<Output = Result<Vec<Enr>, QueryError>> + 'static {
        let channel = self.clone_channel();

        async move {
            let channel = channel.map_err(|_| QueryError::ServiceNotStarted)?;
            let (callback_send, callback_recv) = oneshot::channel();

            let query_kind = QueryKind::FindNode { target_node };

            let event = ServiceRequest::StartQuery(query_kind, callback_send);
            channel
                .send(event)
                .await
                .map_err(|_| QueryError::ChannelFailed("Service channel closed".into()))?;

            callback_recv.await.map_err(|e| QueryError::ChannelFailed(e.to_string()))
        }
    }

    /// Starts a `FIND_NODE` request.
    ///
    /// This will return less than or equal to `num_nodes` ENRs which satisfy the
    /// `predicate`.
    ///
    /// The predicate is a boxed function that takes an ENR reference and returns a boolean
    /// indicating if the record is applicable to the query or not.
    ///
    /// Note: The async syntax is forgone here in order to create `'static` futures, where the
    /// underlying sending channel is cloned.
    ///
    /// ### Example
    /// ```ignore
    ///  let predicate = Box::new(|enr: &Enr| enr.ip().is_some());
    ///  let target = NodeId::random();
    ///  let result = discv5.find_node_predicate(target, predicate, 5).await;
    ///  ```
    pub fn find_node_predicate(
        &self,
        target_node: NodeId,
        predicate: Box<dyn Fn(&Enr) -> bool + Send>,
        target_peer_no: usize,
    ) -> impl Future<Output = Result<Vec<Enr>, QueryError>> + 'static {
        let channel = self.clone_channel();

        async move {
            let channel = channel.map_err(|_| QueryError::ServiceNotStarted)?;
            let (callback_send, callback_recv) = oneshot::channel();

            let query_kind = QueryKind::Predicate { target_node, predicate, target_peer_no };

            let event = ServiceRequest::StartQuery(query_kind, callback_send);
            channel
                .send(event)
                .await
                .map_err(|_| QueryError::ChannelFailed("Service channel closed".into()))?;

            callback_recv.await.map_err(|e| QueryError::ChannelFailed(e.to_string()))
        }
    }

    /// Creates an event stream channel which can be polled to receive Discv5 events.
    pub fn event_stream(
        &self,
    ) -> impl Future<Output = Result<mpsc::Receiver<Event>, Error>> + 'static {
        let channel = self.clone_channel();

        async move {
            let channel = channel?;

            let (callback_send, callback_recv) = oneshot::channel();

            let event = ServiceRequest::RequestEventStream(callback_send);
            channel.send(event).await.map_err(|_| Error::ServiceChannelClosed)?;

            callback_recv.await.map_err(|_| Error::ServiceChannelClosed)
        }
    }

    /// Internal helper function to send events to the Service.
    fn clone_channel(&self) -> Result<mpsc::Sender<ServiceRequest>, Error> {
        if let Some(channel) = self.service_channel.as_ref() {
            Ok(channel.clone())
        } else {
            Err(Error::ServiceNotStarted)
        }
    }
}

impl Drop for Discv5 {
    fn drop(&mut self) {
        self.shutdown();
    }
}
