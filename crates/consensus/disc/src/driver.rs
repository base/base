//! Discovery Module.

use std::net::SocketAddr;

use backon::{ExponentialBuilder, RetryableWithContext};
use base_consensus_peers::{
    BootNode, BootNodes, BootStore, BootStoreFile, EnrValidation, PeerUtils,
};
use derive_more::Debug;
use discv5::{Config, Discv5, Enr, IpMode, enr::NodeId};
use tokio::{
    sync::mpsc::{Sender, channel, error::TrySendError},
    time::{Duration, sleep},
};

use crate::{Discv5Builder, Discv5Handler, HandlerRequest, LocalNode, Metrics};

/// Attempts to forward an ENR to the gossip service without spawning or parking on backpressure.
fn try_forward_enr(enr_sender: &Sender<Enr>, enr: Enr) -> bool {
    match enr_sender.try_send(enr) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            Metrics::discovery_dropped("backpressure").increment(1.0);
            debug!(target: "discovery", "dropping enr because receiver is full");
            false
        }
        Err(TrySendError::Closed(_)) => {
            Metrics::discovery_dropped("closed").increment(1.0);
            debug!(target: "discovery", "dropping enr because receiver is closed");
            false
        }
    }
}

/// The [`Discv5Driver`] drives the discovery service.
///
/// Calling [`Discv5Driver::start`] spawns a new [`Discv5`]
/// discovery service in a new tokio task and returns a
/// [`Discv5Handler`].
///
/// Channels are used to communicate between the [`Discv5Handler`]
/// and the spawned task containing the [`Discv5`] service.
///
/// Message passing gives the driver exclusive access to the discovery service
/// while allowing other tasks to inspect the routing table and ban addresses.
#[derive(Debug)]
pub struct Discv5Driver {
    /// The [`Discv5`] discovery service.
    #[debug(skip)]
    pub disc: Discv5,
    /// The optional [`BootStoreFile`] to use for the bootstore.
    pub bootstore: Option<BootStoreFile>,
    /// Bootnodes used to bootstrap the discovery service.
    pub bootnodes: BootNodes,
    /// The chain ID of the network.
    pub chain_id: u64,
    /// The interval to discovery random nodes.
    pub interval: Duration,
    /// Whether to forward ENRs to the enr receiver on startup.
    pub forward: bool,
    /// The interval at which to store the ENRs in the bootstore.
    /// This is set to 60 seconds by default.
    pub store_interval: Duration,
    /// The frequency at which to remove random nodes from the discovery table.
    /// This is not enabled (`None`) by default.
    pub remove_interval: Option<Duration>,
}

impl Discv5Driver {
    /// Returns a new [`Discv5Builder`] instance.
    pub fn builder(
        local_node: LocalNode,
        chain_id: u64,
        discovery_config: Config,
    ) -> Discv5Builder {
        Discv5Builder::new(local_node, chain_id, discovery_config)
    }

    /// Instantiates a new [`Discv5Driver`].
    pub const fn new(
        disc: Discv5,
        interval: Duration,
        chain_id: u64,
        bootstore: Option<BootStoreFile>,
        bootnodes: BootNodes,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            disc,
            chain_id,
            bootnodes,
            interval,
            forward: true,
            remove_interval: None,
            store_interval: Duration::from_secs(60),
            bootstore,
        })
    }

    /// Whether an unverifiable ENR can be pinged without contacting a different IP.
    ///
    /// Uses the same address-family selection as [`Discv5::send_ping`].
    pub fn should_verify_advertised_socket(
        ip_mode: IpMode,
        enr: &Enr,
        observed: SocketAddr,
    ) -> bool {
        ip_mode.get_contactable_addr(enr).is_some_and(|advertised| {
            advertised.ip().to_canonical() == observed.ip().to_canonical()
        })
    }

    /// Starts the inner [`Discv5`] service.
    async fn init(self) -> Result<Self, discv5::Error> {
        let (s, res) = {
            |mut v: Self| async {
                let res = v.disc.start().await;
                (v, res)
            }
        }
            .retry(ExponentialBuilder::default())
            .context(self)
            .notify(|err: &discv5::Error, dur: Duration| {
                warn!(target: "discovery", error = ?err, duration = ?dur, "Failed to start discovery service");
            })
            .await;
        res.map(|_| s)
    }

    /// Bootstraps the [`Discv5`] table with bootnodes.
    async fn bootstrap_peers(
        bootstore: Option<BootStoreFile>,
        bootnodes: BootNodes,
        chain_id: u64,
        disc: &Discv5,
    ) -> BootStore {
        // Note: if the bootstore file cannot be created, we use a default bootstore.
        let mut store = bootstore
            .map_or_else(BootStore::default, |bootstore| bootstore.try_into().unwrap_or_default());

        let initial_store_length = store.len();

        for bn in bootnodes.0.into_iter().chain(BootNodes::from_chain_id(chain_id).0) {
            let res = match bn {
                BootNode::Enr(enr) => Ok(enr.clone()),
                BootNode::Enode(enode) => disc.request_enr(enode.clone()).await,
            };

            let Ok(enr) = res else {
                debug!(target: "discovery::bootstrap", ?res, "Failed to add boot node ENR to discovery table");
                continue;
            };

            let validation = EnrValidation::validate(&enr, chain_id);
            if validation.is_invalid() {
                trace!(target: "discovery::bootstrap", enr = ?enr, validation = ?validation, "Ignoring Invalid Bootnode ENR");
                continue;
            }

            if let Err(e) = disc.add_enr(enr.clone()) {
                debug!(target: "discovery::bootstrap", error = ?e, "Failed to add enr");
                continue;
            }

            store.add_enr(enr);
        }

        let new_store_len = store.len();

        debug!(target: "discovery::bootstrap",
            added=%(new_store_len - initial_store_length),
            total=%new_store_len,
            "Added new ENRs to discv5 bootstore"
        );

        store
    }

    /// Spawns a new [`Discv5`] discovery service in a new tokio task.
    ///
    /// Returns a [`Discv5Handler`] to communicate with the spawned task.
    pub fn start(mut self) -> (Discv5Handler, tokio::sync::mpsc::Receiver<Enr>) {
        let chain_id = self.chain_id;
        let (req_sender, mut req_recv) = channel::<HandlerRequest>(1024);
        let (enr_sender, enr_recv) = channel::<Enr>(1024);

        tokio::spawn(async move {
            let remove = self.remove_interval.is_some();
            let remove_dur = self.remove_interval.unwrap_or(std::time::Duration::from_secs(600));
            let mut removal_interval = tokio::time::interval(remove_dur);
            let mut interval = tokio::time::interval(self.interval);
            let mut store_interval = tokio::time::interval(self.store_interval);
            let (verified_sender, mut verified_recv) = channel::<Enr>(1024);

            // Step 1: Start the discovery service.
            let Ok(s) = self.init().await else {
                error!(target: "discovery", "Failed to start discovery service");
                return;
            };
            self = s;
            trace!(target: "discovery", "Discv5 Initialized");

            // Step 2: Bootstrap the discovery table with bootnodes.
            let mut store =
                Self::bootstrap_peers(self.bootstore, self.bootnodes, chain_id, &self.disc).await;

            let enrs = self.disc.table_entries_enr();
            info!(target: "discovery", count = enrs.len(), "Discv5 Started");

            // Step 3: Forward ENRs in the bootstore to the enr receiver.
            if self.forward {
                for enr in store.valid_peers_with_chain_id(self.chain_id) {
                    try_forward_enr(&enr_sender, enr.clone());
                }
            }

            // Continuously attempt to start the event stream with a retry limit and shutdown
            // signal.
            let mut retries = 0;
            let max_retries = 10; // Maximum number of retries before giving up.
            let mut event_stream = loop {
                if retries >= max_retries {
                    error!(target: "discovery", "Exceeded maximum retries for event stream startup. Aborting...");
                    return; // Exit the task if the retry limit is reached.
                }
                match self.disc.event_stream().await {
                    Ok(event_stream) => {
                        break event_stream;
                    }
                    Err(e) => {
                        warn!(target: "discovery", error = ?e, "Failed to start event stream");
                        retries += 1;
                        sleep(Duration::from_secs(2)).await;
                        info!(target: "discovery", attempt = retries, max = max_retries, "Retrying event stream startup");
                    }
                }
            };

            // Step 4: Run the core driver loop.
            loop {
                tokio::select! {
                    msg = req_recv.recv() => {
                        match msg {
                            Some(msg) => match msg {
                                HandlerRequest::PeerCount(tx) => {
                                    let peers = self.disc.connected_peers();
                                    if let Err(e) = tx.send(peers) {
                                        warn!(target: "discovery", error = ?e, "Failed to send peer count");
                                    }
                                }
                                HandlerRequest::LocalEnr(tx) => {
                                    let enr = self.disc.local_enr().clone();
                                    if let Err(e) = tx.send(enr.clone()) {
                                        warn!(target: "discovery", error = ?e, "Failed to send local enr");
                                    }
                                }
                                HandlerRequest::TableEnrs(tx) => {
                                    let enrs = self.disc.table_entries_enr();
                                    if let Err(e) = tx.send(enrs) {
                                        warn!(target: "discovery", error = ?e, "Failed to send table enrs");
                                    }
                                },
                                HandlerRequest::TableInfos(tx) => {
                                    let infos = self.disc.table_entries();
                                    if let Err(e) = tx.send(infos) {
                                        warn!(target: "discovery", error = ?e, "Failed to send table infos");
                                    }
                                },
                                HandlerRequest::BanAddrs{addrs_to_ban, ban_duration} => {
                                    let enrs = self.disc.table_entries_enr();

                                    for enr in enrs {
                                        let Some(multi_addr) = PeerUtils::enr_to_multiaddr(&enr) else {
                                            continue;
                                        };

                                        if addrs_to_ban.contains(&multi_addr) {
                                            self.disc.ban_node(&enr.node_id(), Some(ban_duration));
                                        }
                                    }
                                },
                            }
                            None => {
                                trace!(target: "discovery", "Receiver `None` peer enr");
                            }
                        }
                    }
                    event = event_stream.recv() => {
                        let Some(event) = event else {
                            trace!(target: "discovery", "Received `None` event");
                            continue;
                        };
                        match event {
                            discv5::Event::Discovered(enr)
                                if EnrValidation::validate(&enr, chain_id).is_valid() =>
                            {
                                debug!(target: "discovery", enr = ?enr, "Valid ENR discovered, forwarding to swarm");
                                Metrics::discovery_event("discovered").increment(1.0);
                                if try_forward_enr(&enr_sender, enr.clone()) {
                                    store.add_enr(enr);
                                }
                            }
                            discv5::Event::SessionEstablished(enr, addr)
                                if EnrValidation::validate(&enr, chain_id).is_valid() =>
                            {
                                debug!(target: "discovery", addr = ?addr, enr = ?enr, "Session established with valid ENR, forwarding to swarm");
                                Metrics::discovery_event("session_established").increment(1.0);
                                if try_forward_enr(&enr_sender, enr.clone()) {
                                    store.add_enr(enr);
                                }
                            }
                            discv5::Event::UnverifiableEnr { enr, socket, .. }
                                if EnrValidation::validate(&enr, chain_id).is_valid() =>
                            {
                                debug!(target: "discovery", enr = ?enr, "Valid ENR discovered, forwarding to swarm");
                                Metrics::discovery_event("unverifiable_enr").increment(1.0);

                                // discv5 only adds a node to its table after a session whose source
                                // socket matches the node's ENR. Peers behind port-translating NAT
                                // (e.g. K8s pods exposed through a hostPort) fail that check on the
                                // sessions they start, so they are never handed out to other peers.
                                // Ping the advertised socket instead: if it answers, discv5 sets up a
                                // verified session and adds the node to the table.
                                if Self::should_verify_advertised_socket(self.disc.ip_mode(), &enr, socket) {
                                    let ping = self.disc.send_ping(enr.clone());
                                    let (verified_sender, verified) = (verified_sender.clone(), enr.clone());
                                    tokio::spawn(async move {
                                        match ping.await {
                                            Ok(_) => {
                                                debug!(target: "discovery", observed = %socket, "Unverifiable ENR answered ping at its advertised socket");
                                                Metrics::discovery_event("unverifiable_enr_verified").increment(1.0);
                                                let _ = verified_sender.try_send(verified);
                                            }
                                            Err(err) => {
                                                debug!(target: "discovery", observed = %socket, error = ?err, "Unverifiable ENR did not answer ping at its advertised socket");
                                            }
                                        }
                                    });
                                }

                                if try_forward_enr(&enr_sender, enr.clone()) {
                                    store.add_enr(enr);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(enr) = verified_recv.recv() => {
                        // discv5 only inserts a node when a session is established, so a ping
                        // over an existing session would not re-add a node that a session from
                        // another port just removed.
                        if let Err(err) = self.disc.add_enr(enr) {
                            debug!(target: "discovery", error = ?err, "Failed to add verified ENR");
                        }
                    }
                    _ = interval.tick() => {
                        let id = NodeId::random();
                        trace!(target: "discovery", node_id = %id, "Finding random node");
                        Metrics::find_node_request().increment(1.0);
                        let fut = self.disc.find_node(id);
                        let enr_sender = enr_sender.clone();
                        tokio::spawn(async move {
                            match fut.await {
                                Ok(nodes) => {
                                    let enrs = nodes.into_iter().filter(|node| EnrValidation::validate(node, chain_id).is_valid());
                                    for enr in enrs {
                                        try_forward_enr(&enr_sender, enr);
                                    }
                                }
                                Err(err) => {
                                    info!(target: "discovery", error = ?err, "Failed to find node");
                                }
                            }
                        });
                    }
                    _ = store_interval.tick() => {
                        let start = std::time::Instant::now();
                        let enrs = self.disc.table_entries_enr();
                        store.merge(enrs);

                        if let Err(e) = store.sync() {
                            warn!(target: "discovery", error = ?e, "Failed to sync bootstore");
                        }

                        let elapsed = start.elapsed();
                        debug!(target: "discovery", elapsed = ?elapsed, "Bootstore ENRs stored");
                        Metrics::enr_store_time().record(elapsed.as_secs_f64());
                        Metrics::discovery_peer_count().set(self.disc.connected_peers() as f64);
                    }
                    _ = removal_interval.tick() => {
                        if remove {
                            let enrs = self.disc.table_entries_enr();
                            if enrs.len() > 20 {
                                let mut rng = rand::rng();
                                let index = rand::Rng::random_range(&mut rng, 0..enrs.len());
                                let enr = enrs[index].clone();
                                debug!(target: "removal", enr = ?enr, "Removing random ENR");
                                self.disc.remove_node(&enr.node_id());
                            }
                        }
                    }
                }
            }
        });

        (Discv5Handler::new(chain_id, req_sender), enr_recv)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        time::Instant,
    };

    use base_common_chains::ChainConfig;
    use discv5::{
        ConfigBuilder,
        enr::{CombinedKey, CombinedPublicKey, k256::ecdsa::SigningKey},
        handler::NodeContact,
    };
    use tempfile::tempdir;
    use tokio::{net::UdpSocket, sync::mpsc};

    use super::*;
    use crate::LocalNode;

    fn test_enr() -> Enr {
        let key = CombinedKey::generate_secp256k1();
        Enr::builder().build(&key).expect("test enr should build")
    }

    #[test]
    fn try_forward_enr_succeeds_when_channel_has_capacity() {
        let (tx, mut rx) = mpsc::channel(1);
        let enr = test_enr();

        assert!(try_forward_enr(&tx, enr.clone()));

        assert_eq!(rx.try_recv().expect("enr should be forwarded"), enr);
    }

    #[test]
    fn try_forward_enr_drops_when_channel_is_full() {
        let (tx, _rx) = mpsc::channel(1);

        assert!(try_forward_enr(&tx, test_enr()));
        assert!(!try_forward_enr(&tx, test_enr()));
    }

    #[test]
    fn try_forward_enr_drops_when_channel_is_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        assert!(!try_forward_enr(&tx, test_enr()));
    }

    #[tokio::test]
    async fn test_online_discv5_driver() {
        let CombinedKey::Secp256k1(secret_key) = CombinedKey::generate_secp256k1() else {
            unreachable!()
        };

        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let discovery = Discv5Driver::builder(
            LocalNode::new(secret_key, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0, 0),
            ChainConfig::sepolia().chain_id,
            ConfigBuilder::new(socket.into()).build(),
        )
        .build()
        .expect("Failed to build discovery service");
        let (handle, _) = discovery.start();
        assert_eq!(handle.chain_id, ChainConfig::sepolia().chain_id);
    }

    #[tokio::test]
    async fn test_online_discv5_driver_bootstrap_testnet() {
        // Use a test file to make sure bootstore
        // doesn't conflict with a local bootstore.
        let file = tempdir().unwrap();
        let file = file.path().join("bootstore.json");

        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let CombinedKey::Secp256k1(secret_key) = CombinedKey::generate_secp256k1() else {
            unreachable!()
        };
        let mut discovery = Discv5Driver::builder(
            LocalNode::new(secret_key, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0, 0),
            ChainConfig::sepolia().chain_id,
            ConfigBuilder::new(socket.into()).build(),
        )
        .with_bootnodes(BootNodes::testnet())
        .build()
        .expect("Failed to build discovery service");
        discovery.bootstore = Some(BootStoreFile::Custom(file));

        discovery = discovery.init().await.expect("Failed to initialize discovery service");

        // There are no ENRs for Base Sepolia in the bootstore.
        // If an ENR is added, this check will fail.
        Discv5Driver::bootstrap_peers(
            discovery.bootstore,
            discovery.bootnodes,
            ChainConfig::sepolia().chain_id,
            &discovery.disc,
        )
        .await;
        let enrs = discovery.disc.table_entries_enr();
        assert!(enrs.len() >= 2, "Discovery table should have at least 2 ENRs");

        // Filter out testnet ENRs that are not valid.
        let testnet = BootNodes::testnet();
        let testnet: Vec<CombinedPublicKey> = testnet
            .iter()
            .filter_map(|node| match node {
                BootNode::Enr(enr) => {
                    if EnrValidation::validate(enr, ChainConfig::sepolia().chain_id).is_invalid() {
                        return None;
                    }
                    Some(enr.public_key())
                }
                BootNode::Enode(_) => {
                    let node_contact =
                        NodeContact::try_from_multiaddr(node.to_multiaddr().unwrap()).unwrap();
                    Some(node_contact.public_key())
                }
            })
            .collect();

        // `BootNodes::testnet()` returns CL bootnodes only — 2 raw ENRs.
        assert_eq!(testnet.len(), 2);

        // Those ENRs should be in the testnet bootnodes.
        for enr in &enrs {
            assert!(
                testnet.iter().any(|pub_key| pub_key == &enr.public_key()),
                "Discovery table does not contain testnet ENR: {enr:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_online_discv5_driver_bootstrap_mainnet() {
        base_cli_utils::init_test_tracing();

        // Use a test file to make sure bootstore
        // doesn't conflict with a local bootstore.
        let file = tempdir().unwrap();
        let file = file.path().join("bootstore.json");

        // Filter out ENRs that are not valid.
        let mainnet = BootNodes::mainnet();
        let mainnet: Vec<CombinedPublicKey> = mainnet
            .iter()
            .filter_map(|node| match node {
                BootNode::Enr(enr) => {
                    if EnrValidation::validate(enr, ChainConfig::mainnet().chain_id).is_invalid() {
                        return None;
                    }
                    Some(enr.public_key())
                }
                BootNode::Enode(_) => {
                    let node_contact =
                        NodeContact::try_from_multiaddr(node.to_multiaddr().unwrap()).unwrap();
                    Some(node_contact.public_key())
                }
            })
            .collect();

        // `BootNodes::mainnet()` returns CL bootnodes only — 5 raw ENRs.
        assert_eq!(mainnet.len(), 5);

        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

        let CombinedKey::Secp256k1(secret_key) = CombinedKey::generate_secp256k1() else {
            unreachable!()
        };

        let mut discovery = Discv5Driver::builder(
            LocalNode::new(secret_key, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0, 0),
            ChainConfig::mainnet().chain_id,
            ConfigBuilder::new(socket.into()).build(),
        )
        .with_bootnodes(BootNodes::mainnet())
        .build()
        .expect("Failed to build discovery service");
        discovery.bootstore = Some(BootStoreFile::Custom(file));

        discovery = discovery.init().await.expect("Failed to initialize discovery service");

        // There are no Base mainnet ENRs in the bootstore.
        // If an ENR is added, this check will fail.
        Discv5Driver::bootstrap_peers(
            discovery.bootstore,
            discovery.bootnodes,
            ChainConfig::mainnet().chain_id,
            &discovery.disc,
        )
        .await;
        // All 5 CL ENRs are added directly without I/O — no liveness check needed.
        assert!(
            discovery.disc.table_entries_enr().len() >= 5,
            "Discovery table should have at least 5 ENRs"
        );

        // Those ENRs should be in the mainnet bootnodes.
        let disc_enrs = discovery.disc.table_entries_enr();
        for enr in disc_enrs {
            assert!(
                mainnet.iter().any(|pub_key| pub_key == &enr.public_key()),
                "Discovery table does not contain mainnet ENR: {enr:?}"
            );
        }
    }

    #[test]
    fn verifies_only_the_contactable_advertised_socket() {
        let key = CombinedKey::generate_secp256k1();
        let ipv4 = Ipv4Addr::new(192, 0, 2, 1);
        let ipv6: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let dual = Enr::builder().ip4(ipv4).udp4(9000).ip6(ipv6).udp6(9001).build(&key).unwrap();
        let v4_only = Enr::builder().ip4(ipv4).udp4(9000).build(&key).unwrap();
        let v6_only = Enr::builder().ip6(ipv6).udp6(9001).build(&key).unwrap();
        let mapped = Enr::builder().ip6(ipv4.to_ipv6_mapped()).udp6(9001).build(&key).unwrap();
        let no_socket = test_enr();
        let observed_v4 = SocketAddr::from((ipv4, 5000));
        let observed_v6 = SocketAddr::from((ipv6, 5001));
        let observed_mapped = SocketAddr::from((ipv4.to_ipv6_mapped(), 5000));

        for (mode, enr, observed, expected) in [
            (IpMode::Ip4, &dual, observed_v4, true),
            (IpMode::Ip4, &dual, observed_v6, false),
            (IpMode::Ip6, &dual, observed_v4, false),
            (IpMode::Ip6, &dual, observed_v6, true),
            (IpMode::DualStack, &dual, observed_v4, false),
            (IpMode::DualStack, &dual, observed_v6, true),
            (IpMode::DualStack, &v4_only, observed_mapped, true),
            (IpMode::DualStack, &v6_only, observed_v6, true),
            (IpMode::Ip6, &v4_only, observed_v4, false),
            (IpMode::Ip4, &v6_only, observed_v6, false),
            (IpMode::Ip6, &mapped, observed_mapped, false),
            (IpMode::DualStack, &no_socket, observed_v4, false),
            (IpMode::Ip4, &dual, "198.51.100.1:5000".parse().unwrap(), false),
            (IpMode::Ip6, &dual, "[2001:db8::2]:5001".parse().unwrap(), false),
        ] {
            assert_eq!(
                Discv5Driver::should_verify_advertised_socket(mode, enr, observed),
                expected,
                "mode {mode:?}, observed {observed}, enr {enr:?}"
            );
        }
    }

    /// Reserves a free localhost UDP port.
    fn free_udp_port() -> u16 {
        std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
    }

    /// Forwards UDP like a K8s hostPort: datagrams sent to the returned address reach `target`
    /// and its replies leave from the returned address, while packets `target` sends on its own
    /// bypass the forwarder and leave from `target`'s socket.
    async fn spawn_port_forward(target: SocketAddr) -> SocketAddr {
        let public = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let inner = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let public_addr = public.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut inbound, mut outbound) = ([0u8; 2048], [0u8; 2048]);
            let mut client = None;
            loop {
                tokio::select! {
                    Ok((len, from)) = public.recv_from(&mut inbound) => {
                        client = Some(from);
                        let _ = inner.send_to(&inbound[..len], target).await;
                    }
                    Ok((len, _)) = inner.recv_from(&mut outbound) => {
                        if let Some(client) = client {
                            let _ = public.send_to(&outbound[..len], client).await;
                        }
                    }
                }
            }
        });
        public_addr
    }

    /// Starts a driver listening on `listen_port` whose ENR advertises `advertised_port`.
    fn start_driver(
        secret_key: SigningKey,
        listen_port: u16,
        advertised_port: u16,
        bootnodes: BootNodes,
        bootstore: std::path::PathBuf,
    ) -> Discv5Handler {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let listen = SocketAddr::new(localhost, listen_port);
        let (handle, _) = Discv5Driver::builder(
            LocalNode::new(secret_key, localhost, advertised_port, advertised_port),
            TEST_CHAIN_ID,
            ConfigBuilder::new(listen.into()).build(),
        )
        .with_bootnodes(bootnodes)
        .with_bootstore_file(Some(BootStoreFile::Custom(bootstore)))
        .with_interval(Duration::from_millis(500))
        .build()
        .expect("Failed to build discovery service")
        .start();
        handle
    }

    /// Chain ID with no hardcoded bootnodes, so the test never leaves localhost.
    const TEST_CHAIN_ID: u64 = 7_777_777;

    fn generate_key() -> SigningKey {
        let CombinedKey::Secp256k1(secret_key) = CombinedKey::generate_secp256k1() else {
            unreachable!()
        };
        secret_key
    }

    async fn wait_for_table_entry(node: &Discv5Handler, id: NodeId, deadline: Instant) {
        loop {
            let table = node.table_enrs().await.unwrap();
            if table.iter().any(|enr| enr.node_id() == id) {
                return;
            }
            assert!(Instant::now() < deadline, "{id} never entered the table");
            sleep(Duration::from_millis(200)).await;
        }
    }

    #[tokio::test]
    async fn admits_peer_whose_packets_leave_from_a_translated_port() {
        let dir = tempdir().unwrap();

        // Observer, e.g. a bootnode, with a plain socket.
        let observer_port = free_udp_port();
        let observer = start_driver(
            generate_key(),
            observer_port,
            observer_port,
            BootNodes(vec![]),
            dir.path().join("a.json"),
        );
        let observer_enr = observer.local_enr().await.unwrap();

        // Peer behind a hostPort-style forward: it advertises the forwarded port, but the
        // sessions it starts come from its own listen port, so the observer cannot verify
        // its ENR from those sessions.
        let peer_port = free_udp_port();
        let forwarded =
            spawn_port_forward(SocketAddr::from((Ipv4Addr::LOCALHOST, peer_port))).await;
        let peer_key = generate_key();
        let peer = start_driver(
            peer_key.clone(),
            peer_port,
            forwarded.port(),
            BootNodes(vec![BootNode::Enr(observer_enr.clone())]),
            dir.path().join("b.json"),
        );
        let peer_id = peer.local_enr().await.unwrap().node_id();

        wait_for_table_entry(&observer, peer_id, Instant::now() + Duration::from_secs(20)).await;

        // The peer's NAT port changes: a new session from another port fails verification
        // and discv5 removes the peer. The ping that follows travels over the session the
        // observer already has with the advertised socket, so only the driver can re-add it.
        let moved = start_driver(
            peer_key,
            free_udp_port(),
            forwarded.port(),
            BootNodes(vec![BootNode::Enr(observer_enr.clone())]),
            dir.path().join("c.json"),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        wait_for_table_entry(&moved, observer_enr.node_id(), deadline).await;
        // Let the observer process the new session before checking its table.
        sleep(Duration::from_secs(1)).await;
        wait_for_table_entry(&observer, peer_id, Instant::now() + Duration::from_secs(5)).await;
    }
}
