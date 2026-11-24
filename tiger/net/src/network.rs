//! The network: configuration, the peer table, the listener and dialer threads, and peer
//! reputation and bans.
//!
//! Threads: one listener (if listening), one maintenance thread that dials candidates and
//! trusted peers, one thread per peer session, and the discovery thread (if enabled). All
//! counts are bounded by the configuration.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::hex;
use secp256k1::SecretKey;

use crate::{
    crypto::{PeerId, peer_id_of_secret},
    discv5::{Discv5, Discv5Config},
    eth::ETH_CAPABILITIES,
    forkid::ForkSchedule,
    peer::{BlockSource, Peer, SessionConfig, SessionError, handshake, run_session},
    rlpx::{DisconnectReason, LocalCapability, RlpxConnection},
    snap::{SNAP_CAPABILITY, SnapStateSource},
};

/// A node address in enode form: `enode://<128 hex id>@<ip or host>:<tcp port>[?discport=<udp>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRecord {
    /// The node id.
    pub id: PeerId,
    /// The RLPx (TCP) address.
    pub addr: SocketAddr,
    /// The discovery (UDP) port.
    pub udp_port: u16,
}

impl FromStr for NodeRecord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix("enode://").ok_or("missing enode:// prefix")?;
        let (id, rest) = rest.split_once('@').ok_or("missing @")?;
        let id = PeerId::from_slice(&hex::decode(id).map_err(|e| e.to_string())?.try_into().map_err(|_| "id must be 64 bytes".to_string()).map(|v: [u8; 64]| v)?);
        let (host, query) = rest.split_once('?').unwrap_or((rest, ""));
        let addr = host
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("unresolvable host")?;
        let udp_port = match query.strip_prefix("discport=") {
            Some(port) => port.parse().map_err(|_| "bad discport")?,
            None => addr.port(),
        };
        Ok(Self { id, addr, udp_port })
    }
}

impl std::fmt::Display for NodeRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "enode://{}@{}", hex::encode(self.id), self.addr)?;
        if self.udp_port != self.addr.port() {
            write!(f, "?discport={}", self.udp_port)?;
        }
        Ok(())
    }
}

/// Network configuration. The node maps its command-line flags onto it:
/// `--bootnodes` to `bootnodes`, `--trusted-peers` to `trusted_peers`, `--disable-discovery` to
/// `discovery_addr: None`, `--port` to `listen_addr`, `--max-outbound-peers` and
/// `--max-inbound-peers` to the limits.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// The node key (identity).
    pub secret_key: SecretKey,
    /// TCP address for inbound RLPx connections; `None` accepts none.
    pub listen_addr: Option<SocketAddr>,
    /// UDP address for discv5; `None` disables discovery.
    pub discovery_addr: Option<SocketAddr>,
    /// Discovery bootstrap nodes (base-reth's chain bootnodes, or `--bootnodes`). They are also
    /// dial candidates.
    pub bootnodes: Vec<NodeRecord>,
    /// Peers always dialed and kept connected, exempt from peer limits and bans.
    pub trusted_peers: Vec<NodeRecord>,
    /// Most outbound peers (reth's default is 100).
    pub max_outbound: usize,
    /// Most inbound peers (reth's default is 30).
    pub max_inbound: usize,
    /// Most dials in flight at once.
    pub max_concurrent_dials: usize,
    /// The chain id, sent as the eth network id.
    pub network_id: u64,
    /// The fork schedule for Status and fork id checks.
    pub forks: ForkSchedule,
    /// The Hello client id.
    pub client_id: String,
    /// The IP address to advertise in the discovery record, if known.
    pub external_ip: Option<IpAddr>,
}

impl NetworkConfig {
    /// A configuration with reth's default limits.
    pub fn new(secret_key: SecretKey, network_id: u64, forks: ForkSchedule) -> Self {
        Self {
            secret_key,
            listen_addr: Some(SocketAddr::from(([0, 0, 0, 0], 30303))),
            discovery_addr: Some(SocketAddr::from(([0, 0, 0, 0], 30303))),
            bootnodes: Vec::new(),
            trusted_peers: Vec::new(),
            max_outbound: 100,
            max_inbound: 30,
            max_concurrent_dials: 16,
            network_id,
            forks,
            client_id: format!("tiger/v{}", env!("CARGO_PKG_VERSION")),
            external_ip: None,
        }
    }
}

/// How bad a peer's behaviour was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Penalty {
    /// The peer did not answer in time.
    Timeout,
    /// An answer that was empty although the peer should have the data.
    Useless,
    /// An answer that fails validation (wrong hashes, bad proof): banned at once.
    BadResponse,
}

/// Reputation at or below which a peer is banned.
const BAN_THRESHOLD: i32 = -100;
/// How long a ban lasts.
const BAN_DURATION: Duration = Duration::from_secs(60 * 60);
/// How long a failed dial target waits before being dialed again.
const DIAL_BACKOFF: Duration = Duration::from_secs(30);
/// Most remembered reputations and candidates (bounded memory).
const MAX_REMEMBERED: usize = 4096;

#[derive(Debug, Default)]
struct Reputation {
    score: i32,
    banned_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct Table {
    peers: HashMap<PeerId, Arc<Peer>>,
    dialing: HashSet<PeerId>,
    reputation: HashMap<PeerId, Reputation>,
    candidates: VecDeque<NodeRecord>,
    last_dial: HashMap<PeerId, Instant>,
}

impl Table {
    fn is_banned(&self, id: &PeerId) -> bool {
        self.reputation
            .get(id)
            .and_then(|r| r.banned_until)
            .is_some_and(|until| until > Instant::now())
    }

    fn counts(&self) -> (usize, usize) {
        let inbound = self.peers.values().filter(|p| p.inbound && !p.trusted).count();
        let outbound = self.peers.values().filter(|p| !p.inbound && !p.trusted).count();
        (inbound, outbound)
    }
}

pub(crate) struct Inner {
    config: NetworkConfig,
    session: SessionConfig,
    local_caps: Vec<LocalCapability>,
    chain: Arc<dyn BlockSource>,
    snap_source: Option<Arc<dyn SnapStateSource>>,
    table: Mutex<Table>,
    shutdown: AtomicBool,
    dials: AtomicUsize,
    discovery: Option<Discv5>,
}

/// A running network. Cloning shares it; [`Network::shutdown`] stops it.
#[derive(Clone)]
pub struct Network {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Network").field("id", &self.local_id()).finish()
    }
}

/// A summary of one connected peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Node id.
    pub id: PeerId,
    /// Remote address.
    pub addr: SocketAddr,
    /// Client name.
    pub client_id: String,
    /// Negotiated eth version.
    pub eth_version: u64,
    /// Whether snap was negotiated.
    pub snap: bool,
    /// Last announced head number (0 if unknown).
    pub head_number: u64,
    /// Whether the peer dialed us.
    pub inbound: bool,
}

impl Network {
    /// Starts the network threads.
    pub fn start(
        config: NetworkConfig,
        chain: Arc<dyn BlockSource>,
        snap_source: Option<Arc<dyn SnapStateSource>>,
    ) -> io::Result<Self> {
        // snap/2 is always offered so snap sync can download; without a state source, requests
        // from peers get empty answers.
        let mut local_caps = ETH_CAPABILITIES.to_vec();
        local_caps.push(SNAP_CAPABILITY);
        let session = SessionConfig {
            network_id: config.network_id,
            forks: config.forks.clone(),
            handshake_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
        };
        let listener = match config.listen_addr {
            Some(addr) => Some(TcpListener::bind(addr)?),
            None => None,
        };
        let tcp_port = listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(0);
        let discovery = match config.discovery_addr {
            Some(addr) => {
                let head = chain.chain_head();
                let fork_id = config.forks.fork_id(crate::forkid::Head {
                    number: head.number,
                    timestamp: head.timestamp,
                });
                Some(Discv5::start(Discv5Config {
                    secret_key: config.secret_key,
                    listen_addr: addr,
                    tcp_port,
                    external_ip: config.external_ip,
                    fork_id,
                    bootnodes: config.bootnodes.clone(),
                })?)
            }
            None => None,
        };
        let mut table = Table::default();
        table.candidates.extend(config.bootnodes.iter().copied());
        let inner = Arc::new(Inner {
            config,
            session,
            local_caps,
            chain,
            snap_source,
            table: Mutex::new(table),
            shutdown: AtomicBool::new(false),
            dials: AtomicUsize::new(0),
            discovery,
        });
        if let Some(listener) = listener {
            listener.set_nonblocking(true)?;
            let inner = inner.clone();
            thread::Builder::new()
                .name("tiger-net-listen".into())
                .spawn(move || listen(inner, listener))?;
        }
        {
            let inner = inner.clone();
            thread::Builder::new()
                .name("tiger-net-dial".into())
                .spawn(move || maintain(inner))?;
        }
        Ok(Self { inner })
    }

    /// This node's id.
    pub fn local_id(&self) -> PeerId {
        peer_id_of_secret(&self.inner.config.secret_key)
    }

    /// The connected peers.
    pub fn peers(&self) -> Vec<PeerInfo> {
        let table = self.inner.table.lock().unwrap();
        table
            .peers
            .values()
            .map(|p| PeerInfo {
                id: p.id,
                addr: p.addr,
                client_id: p.client_id.clone(),
                eth_version: p.eth_version(),
                snap: p.supports_snap(),
                head_number: p.head().0,
                inbound: p.inbound,
            })
            .collect()
    }

    /// The number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.inner.table.lock().unwrap().peers.len()
    }

    /// Adds a dial candidate (for example from an admin call or a test).
    pub fn add_peer(&self, record: NodeRecord) {
        let mut table = self.inner.table.lock().unwrap();
        table.last_dial.remove(&record.id);
        table.candidates.push_front(record);
        table.candidates.truncate(MAX_REMEMBERED);
    }

    /// A connected peer by id.
    pub fn peer(&self, id: &PeerId) -> Option<Arc<Peer>> {
        self.inner.table.lock().unwrap().peers.get(id).cloned()
    }

    /// The best peer for a new request: eth (and snap if `snap`), not closed, fewest requests
    /// in flight, and at most `max_inflight` of them. Peers in `exclude` are skipped.
    pub fn select_peer(
        &self,
        snap: bool,
        max_inflight: usize,
        exclude: &[PeerId],
    ) -> Option<Arc<Peer>> {
        let table = self.inner.table.lock().unwrap();
        table
            .peers
            .values()
            .filter(|p| !p.is_closed() && (!snap || p.supports_snap()))
            .filter(|p| p.inflight() < max_inflight && !exclude.contains(&p.id))
            .min_by_key(|p| (p.inflight(), std::cmp::Reverse(table.reputation.get(&p.id).map_or(0, |r| r.score))))
            .cloned()
    }

    /// Lowers a peer's reputation; disconnects and bans it once it falls below the threshold.
    pub fn report(&self, id: &PeerId, penalty: Penalty) {
        let mut table = self.inner.table.lock().unwrap();
        let trusted = self.inner.config.trusted_peers.iter().any(|t| t.id == *id);
        let reputation = table.reputation.entry(*id).or_default();
        reputation.score += match penalty {
            Penalty::Timeout => -25,
            Penalty::Useless => -10,
            Penalty::BadResponse => BAN_THRESHOLD,
        };
        if reputation.score <= BAN_THRESHOLD && !trusted {
            reputation.banned_until = Some(Instant::now() + BAN_DURATION);
            reputation.score = 0;
            if let Some(peer) = table.peers.get(id) {
                peer.close(DisconnectReason::ProtocolBreach);
            }
        }
        if table.reputation.len() > MAX_REMEMBERED {
            let now = Instant::now();
            table.reputation.retain(|_, r| r.banned_until.is_some_and(|u| u > now));
        }
    }

    /// Stops all threads and disconnects every peer.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        for peer in self.inner.table.lock().unwrap().peers.values() {
            peer.close(DisconnectReason::ClientQuitting);
        }
        if let Some(discovery) = &self.inner.discovery {
            discovery.shutdown();
        }
    }

    /// The local discovery record (ENR) in text form, if discovery runs.
    pub fn local_enr(&self) -> Option<String> {
        self.inner.discovery.as_ref().map(|d| d.local_enr_text())
    }
}

fn listen(inner: Arc<Inner>, listener: TcpListener) {
    while !inner.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (inbound, _) = inner.table.lock().unwrap().counts();
                if inbound >= inner.config.max_inbound {
                    // Still let trusted peers in: they are identified only after the handshake,
                    // so accept, and the session drops non-trusted peers over the limit.
                }
                let _ = stream.set_nonblocking(false);
                let inner = inner.clone();
                let _ = thread::Builder::new().name("tiger-net-peer".into()).spawn(move || {
                    let conn = RlpxConnection::accept(
                        stream,
                        &inner.config.secret_key,
                        &inner.local_caps,
                        &inner.config.client_id,
                        Duration::from_secs(10),
                    );
                    if let Ok(conn) = conn {
                        start_session(&inner, conn, true);
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn start_session(inner: &Arc<Inner>, conn: RlpxConnection, inbound: bool) {
    let remote = conn.remote_id;
    let trusted = inner.config.trusted_peers.iter().any(|t| t.id == remote);
    {
        let table = inner.table.lock().unwrap();
        let (inbound_count, outbound_count) = table.counts();
        let full = if inbound {
            inbound_count >= inner.config.max_inbound
        } else {
            outbound_count >= inner.config.max_outbound
        };
        let reason = if table.peers.contains_key(&remote) || remote == peer_id_of_secret(&inner.config.secret_key) {
            Some(DisconnectReason::AlreadyConnected)
        } else if table.is_banned(&remote) && !trusted {
            Some(DisconnectReason::UselessPeer)
        } else if full && !trusted {
            Some(DisconnectReason::TooManyPeers)
        } else {
            None
        };
        if let Some(reason) = reason {
            let mut writer = conn.writer;
            let _ = writer.disconnect(reason);
            return;
        }
    }
    let (peer, reader) =
        match handshake(conn, &inner.session, inner.chain.as_ref(), trusted, inbound) {
            Ok(result) => result,
            Err(SessionError::Incompatible(_)) => {
                // Another chain or fork: do not dial it again for a while.
                let mut table = inner.table.lock().unwrap();
                table.reputation.entry(remote).or_default().banned_until =
                    Some(Instant::now() + BAN_DURATION);
                return;
            }
            Err(_) => return,
        };
    {
        let mut table = inner.table.lock().unwrap();
        if table.peers.contains_key(&remote) {
            peer.close(DisconnectReason::AlreadyConnected);
            return;
        }
        table.peers.insert(remote, peer.clone());
    }
    let error = run_session(
        &peer,
        reader,
        &inner.session,
        inner.chain.as_ref(),
        inner.snap_source.as_deref(),
        &inner.shutdown,
    );
    let mut table = inner.table.lock().unwrap();
    table.peers.remove(&remote);
    if matches!(error, SessionError::Breach(_) | SessionError::Incompatible(_)) && !trusted {
        table.reputation.entry(remote).or_default().banned_until =
            Some(Instant::now() + BAN_DURATION);
    }
}

fn maintain(inner: Arc<Inner>) {
    while !inner.shutdown.load(Ordering::Relaxed) {
        if let Some(discovery) = &inner.discovery {
            let found = discovery.take_found();
            let head = inner.chain.chain_head();
            let head = crate::forkid::Head { number: head.number, timestamp: head.timestamp };
            let mut table = inner.table.lock().unwrap();
            for node in found {
                // Skip records that advertise an incompatible fork; records without one are
                // checked at the Status exchange.
                if node.fork_id.is_some_and(|id| inner.config.forks.validate(head, id).is_err()) {
                    continue;
                }
                let record = node.record;
                if !table.candidates.contains(&record) {
                    table.candidates.push_back(record);
                }
            }
            while table.candidates.len() > MAX_REMEMBERED {
                table.candidates.pop_front();
            }
        }
        let mut to_dial = Vec::new();
        {
            let mut table = inner.table.lock().unwrap();
            let now = Instant::now();
            let local = peer_id_of_secret(&inner.config.secret_key);
            for trusted in &inner.config.trusted_peers {
                let recent = table.last_dial.get(&trusted.id).is_some_and(|t| now - *t < Duration::from_secs(5));
                if !table.peers.contains_key(&trusted.id) && !table.dialing.contains(&trusted.id) && !recent {
                    to_dial.push(*trusted);
                }
            }
            let (_, outbound) = table.counts();
            let free = inner
                .config
                .max_outbound
                .saturating_sub(outbound + table.dialing.len())
                .min(inner.config.max_concurrent_dials.saturating_sub(inner.dials.load(Ordering::Relaxed)));
            let mut rotated = 0;
            while to_dial.len() < free && rotated < table.candidates.len() {
                let Some(candidate) = table.candidates.pop_front() else { break };
                rotated += 1;
                let backoff = table.last_dial.get(&candidate.id).is_some_and(|t| now - *t < DIAL_BACKOFF);
                let skip = candidate.id == local ||
                    table.peers.contains_key(&candidate.id) ||
                    table.dialing.contains(&candidate.id) ||
                    table.is_banned(&candidate.id) ||
                    backoff;
                table.candidates.push_back(candidate);
                if !skip && !to_dial.iter().any(|d: &NodeRecord| d.id == candidate.id) {
                    to_dial.push(candidate);
                }
            }
            for record in &to_dial {
                table.dialing.insert(record.id);
                table.last_dial.insert(record.id, now);
            }
            if table.last_dial.len() > MAX_REMEMBERED {
                table.last_dial.retain(|_, t| now - *t < DIAL_BACKOFF);
            }
        }
        for record in to_dial {
            inner.dials.fetch_add(1, Ordering::Relaxed);
            let inner = inner.clone();
            let _ = thread::Builder::new().name("tiger-net-peer".into()).spawn(move || {
                let conn = RlpxConnection::connect(
                    record.addr,
                    record.id,
                    &inner.config.secret_key,
                    &inner.local_caps,
                    &inner.config.client_id,
                    Duration::from_secs(10),
                );
                inner.dials.fetch_sub(1, Ordering::Relaxed);
                inner.table.lock().unwrap().dialing.remove(&record.id);
                if let Ok(conn) = conn {
                    start_session(&inner, conn, false);
                }
            });
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// A channel for collecting responses from several peers at once.
pub fn response_channel() -> (mpsc::Sender<crate::peer::Response>, mpsc::Receiver<crate::peer::Response>) {
    mpsc::channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enode_urls() {
        let text = "enode://87a32fd13bd596b2ffca97020e31aef4ddcc1bbd4b95bb633d16c1329f654f34049ed240a36b449fda5e5225d70fe40bc667f53c304b71f8e68fc9d448690b51@3.231.138.188:30301";
        let record: NodeRecord = text.parse().unwrap();
        assert_eq!(record.addr, "3.231.138.188:30301".parse().unwrap());
        assert_eq!(record.udp_port, 30301);
        assert_eq!(record.to_string(), text);
        let with_disc: NodeRecord =
            format!("{}?discport=9200", text.replace(":30301", ":30303")).parse().unwrap();
        assert_eq!((with_disc.addr.port(), with_disc.udp_port), (30303, 9200));
        assert!("enode://00@1.2.3.4:1".parse::<NodeRecord>().is_err());
    }
}
