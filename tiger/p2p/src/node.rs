//! The tp2p protocol logic as a sans-IO state machine: peers, Status, ping, peer exchange,
//! request routing with limits, and validated gossip. It never touches a socket or a clock:
//! callers pass the time in milliseconds and feed it connection events and decoded messages,
//! then drain [`Output`]s. The thread driver (`driver.rs`) and the simulator run the same code.
//!
//! Every queue and map is bounded, every peer action has a rate limit, and every wait has a
//! deadline. Gossip is forwarded only after the application reports [`Validation::Accept`].

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::SocketAddr,
};

use secp256k1::{PublicKey, SECP256K1, SecretKey};

use crate::{
    table::{PeerId, PeerTable, group},
    wire::{
        DisconnectReason, MAX_IDS, MAX_PEER_RECORDS, Message, MessageId, PROTOCOL_VERSION,
        PeerRecord, ResponseCode, Status,
    },
};

/// A deterministic random number generator (SplitMix64), seeded by the caller.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A uniform value below `n` (`n > 0`).
    pub const fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        self.next_u64() % n
    }
}

/// Which network a node is on. Both the handshake prologue and record signatures bind it, so
/// nodes of another chain, genesis or layer cannot connect or replay records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    /// The L2 chain id.
    pub chain_id: u64,
    /// The L2 genesis block hash.
    pub genesis: [u8; 32],
    /// [`Network::EXECUTION`] or [`Network::CONSENSUS`].
    pub layer: u8,
}

impl Network {
    /// The execution layer network (tiger nodes: blocks, headers, witnesses).
    pub const EXECUTION: u8 = 1;
    /// The consensus layer network (consensus nodes: unsafe payload gossip).
    pub const CONSENSUS: u8 = 2;

    /// The Noise prologue: `"tp2p/1" || chain_id (u64 big-endian) || genesis (32) || layer (1)`,
    /// 47 bytes.
    pub fn prologue(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(47);
        p.extend_from_slice(b"tp2p/1");
        p.extend_from_slice(&self.chain_id.to_be_bytes());
        p.extend_from_slice(&self.genesis);
        p.push(self.layer);
        p
    }

    /// The network id: SHA-256 of the prologue. Peer records sign it.
    pub fn id(&self) -> [u8; 32] {
        crate::noise::sha256(&[&self.prologue()])
    }
}

/// A kind of request the application serves or makes, with its limits.
#[derive(Debug, Clone, Copy)]
pub struct RequestKind {
    /// The kind on the wire.
    pub kind: u16,
    /// The capability bit a peer must advertise to be asked.
    pub capability: u32,
    /// The largest request body.
    pub max_request: usize,
    /// The largest response body.
    pub max_response: usize,
    /// Requests of this kind one peer may make per second (token bucket of the same burst).
    pub per_peer_per_second: u32,
    /// How long to wait for the answer (ms).
    pub timeout_ms: u64,
}

/// A kind of one-hop notice, with its size limit.
#[derive(Debug, Clone, Copy)]
pub struct NotifyKind {
    /// The kind on the wire.
    pub kind: u16,
    /// The largest body.
    pub max_len: usize,
    /// Notices of this kind one peer may send per second.
    pub per_peer_per_second: u32,
}

/// Node configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The network.
    pub network: Network,
    /// The node's static key.
    pub secret: SecretKey,
    /// The address others should dial, advertised in the node's signed record.
    pub advertise: Option<SocketAddr>,
    /// Capability bits this node advertises.
    pub capabilities: u32,
    /// Outbound connections to keep.
    pub max_outbound: usize,
    /// Inbound connections to accept.
    pub max_inbound: usize,
    /// Fixed peers to dial first and keep dialing.
    pub bootnodes: Vec<(PublicKey, SocketAddr)>,
    /// Peers never banned or refused for capacity.
    pub trusted: Vec<PublicKey>,
    /// Peers that get a small gossip message in full; the others get only its id.
    pub gossip_fanout: usize,
    /// Peers that get a gossip message of at least [`Config::large_gossip`] bytes in full. Pushing
    /// large messages to many peers mostly sends duplicates (every node then receives about this
    /// many copies); announcing them costs a round trip per hop instead (README.md, "Gossip").
    pub gossip_fanout_large: usize,
    /// The size from which a gossip message counts as large.
    pub large_gossip: usize,
    /// The largest gossip message.
    pub max_gossip_len: usize,
    /// Gossip messages one peer may send per second (burst twice that).
    pub gossip_per_second: u32,
    /// Request kinds.
    pub requests: Vec<RequestKind>,
    /// Notice kinds.
    pub notices: Vec<NotifyKind>,
    /// Seed of the node's random choices.
    pub seed: u64,
}

impl Config {
    /// Defaults for a network: 8 outbound and 32 inbound peers, fanout 6 (1 from 16 KiB), 10 MiB
    /// gossip at most,
    /// 50 gossip messages per peer per second, no request or notice kinds.
    pub fn new(network: Network, secret: SecretKey, seed: u64) -> Self {
        Self {
            network,
            secret,
            advertise: None,
            capabilities: 0,
            max_outbound: 8,
            max_inbound: 32,
            bootnodes: Vec::new(),
            trusted: Vec::new(),
            gossip_fanout: 6,
            gossip_fanout_large: 1,
            large_gossip: 16 * 1024,
            max_gossip_len: 10 * 1024 * 1024,
            gossip_per_second: 50,
            requests: Vec::new(),
            notices: Vec::new(),
            seed,
        }
    }

    /// The largest body a message of `type_byte` may declare; the framing refuses larger ones
    /// before buffering them.
    pub fn message_limit(&self, type_byte: u8) -> usize {
        let request = self.requests.iter().map(|k| k.max_request).max().unwrap_or(0);
        let response = self.requests.iter().map(|k| k.max_response).max().unwrap_or(0);
        let notice = self.notices.iter().map(|k| k.max_len).max().unwrap_or(0);
        match type_byte {
            0x05 => 4 + MAX_PEER_RECORDS * 130,
            0x10 => 6 + request,
            0x11 => 5 + response,
            0x20 => 1 + self.max_gossip_len,
            0x21 | 0x22 | 0x24 => 4 + MAX_IDS * 20,
            0x23 => 2 + notice,
            _ => 64,
        }
    }
}

/// The application's verdict on a gossip message, as in gossipsub's message validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// Valid: deliver and forward it.
    Accept,
    /// Not forwarded, no penalty (a duplicate, or not useful).
    Ignore,
    /// Invalid: not forwarded, and the sender is penalized.
    Reject,
}

/// Penalties the application reports for what a peer sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Penalty {
    /// A response that failed the application's checks (a bad block or witness).
    BadResponse,
    /// A response that was slow or useless.
    Useless,
}

/// A local request handle.
pub type RequestId = u64;

/// Why a request got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// No ready peer offers the request's capability.
    NoPeer,
    /// The peer did not answer in time.
    Timeout,
    /// The peer disconnected first.
    Disconnected,
    /// The peer answered with this code.
    Refused(ResponseCode),
}

/// Whom to send a request to.
#[derive(Debug, Clone)]
pub enum Target {
    /// The best ready peer with the capability.
    Any,
    /// The best ready peer with the capability, other than these.
    AnyExcept(Vec<PeerId>),
    /// This peer.
    Peer(PeerId),
}

/// What happened, for the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A peer finished Status and is ready.
    PeerReady {
        /// The peer.
        peer: PeerId,
        /// Its Status.
        status: Status,
    },
    /// A ready peer is gone.
    PeerGone {
        /// The peer.
        peer: PeerId,
    },
    /// A new gossip message; answer with [`Node::validated`].
    Gossip {
        /// The message id.
        id: MessageId,
        /// Its topic.
        topic: u8,
        /// The message.
        data: Vec<u8>,
        /// The peer it came from.
        from: PeerId,
    },
    /// A peer's request; answer with [`Node::respond`].
    Request {
        /// The peer.
        peer: PeerId,
        /// The request id to answer.
        id: u32,
        /// The kind.
        kind: u16,
        /// The body.
        body: Vec<u8>,
    },
    /// The answer to one of our requests.
    Response {
        /// The request.
        request: RequestId,
        /// The peer it went to (all zero for `NoPeer`).
        peer: PeerId,
        /// The body, or why there is none.
        result: Result<Vec<u8>, RequestError>,
    },
    /// A peer's one-hop notice.
    Notify {
        /// The peer.
        peer: PeerId,
        /// The kind.
        kind: u16,
        /// The body.
        body: Vec<u8>,
    },
}

/// A connection, numbered by the caller.
pub type ConnId = u64;

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Send a message on a connection.
    Send {
        /// The connection.
        conn: ConnId,
        /// The message.
        message: Message,
    },
    /// Send `Disconnect(reason)` if possible, then close the connection.
    Close {
        /// The connection.
        conn: ConnId,
        /// Why.
        reason: DisconnectReason,
    },
    /// Open an outbound connection and report it with [`Node::connected`] or
    /// [`Node::dial_failed`].
    Dial {
        /// The peer's key (the handshake's expected responder).
        key: PublicKey,
        /// Where.
        addr: SocketAddr,
    },
    /// Something for the application.
    Event(Event),
}

/// Which side opened a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// We dialed.
    Outbound,
    /// The peer dialed.
    Inbound,
}

const PING_INTERVAL_MS: u64 = 15_000;
const PING_TIMEOUT_MS: u64 = 20_000;
const STATUS_TIMEOUT_MS: u64 = 10_000;
const PEX_INTERVAL_MS: u64 = 30_000;
const PEX_ANSWER_LIMIT: usize = 32;
const PEX_MIN_INTERVAL_MS: u64 = 10_000;
const DIAL_INTERVAL_MS: u64 = 1_000;
const MAX_PENDING_DIALS: usize = 4;
const SEEN_TTL_MS: u64 = 120_000;
const MAX_SEEN: usize = 16_384;
const CACHE_TTL_MS: u64 = 15_000;
const MAX_PENDING_VALIDATION: usize = 1_024;
const VALIDATION_TIMEOUT_MS: u64 = 10_000;
const WANT_TIMEOUT_MS: u64 = 500;
const MAX_WANT_ALTERNATES: usize = 8;
const MAX_WANTS: usize = 4_096;
const MAX_INFLIGHT_PER_PEER: usize = 32;
const MAX_INBOUND_INFLIGHT_PER_PEER: usize = 64;
const IHAVE_IDS_PER_SECOND: u32 = 1_000;
/// Gossip messages at least this long get an IDontWant to the other peers on arrival.
const IDONTWANT_MIN_LEN: usize = 1_024;
const BAN_MS: u64 = 3_600_000;
const MAX_REFUSING: usize = 8;
const MAX_INBOUND_PER_GROUP: usize = 4;
const REFUSE_GRACE_MS: u64 = 2_000;
/// Score at or below which a peer is disconnected and banned.
pub const BAN_SCORE: i32 = -100;
const REJECT_PENALTY: i32 = 25;
const BAD_RESPONSE_PENALTY: i32 = 50;
const TIMEOUT_PENALTY: i32 = 5;
const USELESS_PENALTY: i32 = 2;
const RATE_PENALTY: i32 = 1;

/// A token bucket refilled continuously.
#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    at: u64,
}

impl Bucket {
    fn new(per_second: u32, burst: u32, now: u64) -> Self {
        Self {
            tokens: f64::from(burst),
            rate: f64::from(per_second) / 1000.0,
            burst: f64::from(burst),
            at: now,
        }
    }

    fn take(&mut self, n: f64, now: u64) -> bool {
        self.tokens =
            (self.tokens + (now.saturating_sub(self.at)) as f64 * self.rate).min(self.burst);
        self.at = now;
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
struct Outstanding {
    local: RequestId,
    kind: u16,
    deadline: u64,
}

#[derive(Debug)]
struct Peer {
    conn: ConnId,
    key: PublicKey,
    id: PeerId,
    addr: SocketAddr,
    direction: Direction,
    status: Option<Status>,
    connected_at: u64,
    score: i32,
    score_at: u64,
    last_recv: u64,
    ping: Option<(u64, u64)>,
    last_ping: u64,
    rtt_ms: u64,
    next_wire_id: u32,
    outstanding: BTreeMap<u32, Outstanding>,
    inbound_requests: HashSet<u32>,
    request_buckets: HashMap<u16, Bucket>,
    notice_buckets: HashMap<u16, Bucket>,
    gossip_bucket: Bucket,
    ihave_bucket: Bucket,
    asked_peers: bool,
    last_pex_answer: u64,
    trusted: bool,
    /// Set for an inbound peer accepted over capacity: it gets one peer-exchange answer (so a
    /// refused node still learns where to go) and is closed at this time at the latest.
    refuse_at: Option<u64>,
}

impl Peer {
    const fn ready(&self) -> bool {
        self.status.is_some() && self.refuse_at.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeenState {
    Pending,
    Accepted,
    Done,
}

#[derive(Debug)]
struct Seen {
    state: SeenState,
    at: u64,
    topic: u8,
    data: Option<Vec<u8>>,
    from: Option<ConnId>,
    have: Vec<ConnId>,
}

#[derive(Debug)]
struct Want {
    asked: ConnId,
    deadline: u64,
    alternates: Vec<ConnId>,
}

/// Counters for measurements and simulations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Gossip messages received in full, including duplicates.
    pub gossip_received: u64,
    /// Of those, duplicates.
    pub gossip_duplicates: u64,
    /// Gossip messages sent in full.
    pub gossip_sent: u64,
    /// Ids announced.
    pub ihave_sent: u64,
    /// Ids asked for.
    pub iwant_sent: u64,
    /// Peers banned.
    pub bans: u64,
    /// Connections closed for protocol violations.
    pub violations: u64,
}

/// The protocol state machine.
#[derive(Debug)]
pub struct Node {
    config: Config,
    network_id: [u8; 32],
    local_id: PeerId,
    record: Option<PeerRecord>,
    rng: Rng,
    // Ordered maps where iteration order reaches outputs or the RNG, so a run is a function of
    // the seed alone (the simulator depends on it).
    peers: BTreeMap<ConnId, Peer>,
    by_id: HashMap<PeerId, ConnId>,
    table: PeerTable,
    /// Dials in flight: when each started and the group it targets.
    pending_dials: HashMap<PeerId, (u64, u64)>,
    seen: HashMap<MessageId, Seen>,
    seen_order: VecDeque<(u64, MessageId)>,
    pending_validation: usize,
    wants: BTreeMap<MessageId, Want>,
    requests: HashMap<RequestId, (ConnId, u32)>,
    next_request: RequestId,
    head: (u64, [u8; 32]),
    last_dial: u64,
    last_pex: u64,
    outputs: VecDeque<Output>,
    stats: Stats,
}

/// Bytes of the SHA-256 of `topic || data` that form a gossip id.
pub fn message_id(topic: u8, data: &[u8]) -> MessageId {
    let digest = crate::noise::sha256(&[&[topic], data]);
    digest[..20].try_into().expect("20 bytes")
}

impl Node {
    /// A node at time `now` (ms).
    pub fn new(config: Config, now: u64) -> Self {
        let mut rng = Rng::new(config.seed);
        let network_id = config.network.id();
        let local_id = PublicKey::from_secret_key(SECP256K1, &config.secret).serialize();
        let record = config.advertise.map(|addr| {
            PeerRecord::signed(&config.secret, &network_id, addr, now.max(1), config.capabilities)
        });
        let table = PeerTable::new(rng.next_u64(), &config.bootnodes);
        Self {
            config,
            network_id,
            local_id,
            record,
            rng,
            peers: BTreeMap::new(),
            by_id: HashMap::new(),
            table,
            pending_dials: HashMap::new(),
            seen: HashMap::new(),
            seen_order: VecDeque::new(),
            pending_validation: 0,
            wants: BTreeMap::new(),
            requests: HashMap::new(),
            next_request: 1,
            head: (0, [0; 32]),
            last_dial: 0,
            last_pex: now,
            outputs: VecDeque::new(),
            stats: Stats::default(),
        }
    }

    /// The configuration.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// This node's id.
    pub const fn local_id(&self) -> PeerId {
        self.local_id
    }

    /// Counters.
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// The peer table.
    pub const fn table(&self) -> &PeerTable {
        &self.table
    }

    /// Ready peers with their address and direction.
    pub fn peers(&self) -> Vec<(PeerId, SocketAddr, Direction)> {
        self.peers.values().filter(|p| p.ready()).map(|p| (p.id, p.addr, p.direction)).collect()
    }

    /// A ready peer's Status.
    pub fn peer_status(&self, peer: &PeerId) -> Option<&Status> {
        self.by_id.get(peer).and_then(|c| self.peers.get(c)).and_then(|p| p.status.as_ref())
    }

    /// The next thing to do, if any.
    pub fn poll(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// Sets the head reported in Status to peers that connect from now on.
    pub const fn set_head(&mut self, number: u64, hash: [u8; 32]) {
        self.head = (number, hash);
    }

    fn send(&mut self, conn: ConnId, message: Message) {
        self.outputs.push_back(Output::Send { conn, message });
    }

    fn event(&mut self, event: Event) {
        self.outputs.push_back(Output::Event(event));
    }

    fn is_trusted(&self, key: &PublicKey) -> bool {
        self.config.trusted.contains(key)
    }

    /// A connection finished its handshake with the peer `key`. Returns the disconnect reason if
    /// the node refuses it (the caller then closes it); otherwise the node sends Status.
    pub fn connected(
        &mut self,
        conn: ConnId,
        key: PublicKey,
        addr: SocketAddr,
        direction: Direction,
        now: u64,
    ) -> Result<(), DisconnectReason> {
        let id = key.serialize();
        if direction == Direction::Outbound {
            self.pending_dials.remove(&id);
        }
        let trusted = self.is_trusted(&key);
        if id == self.local_id {
            return Err(DisconnectReason::Duplicate);
        }
        if !trusted && self.table.is_banned(&id, &addr.ip(), now) {
            return Err(DisconnectReason::Misbehaving);
        }
        if self.by_id.contains_key(&id) {
            return Err(DisconnectReason::Duplicate);
        }
        let count = self
            .peers
            .values()
            .filter(|p| p.direction == direction && p.refuse_at.is_none())
            .count();
        let limit = match direction {
            Direction::Outbound => self.config.max_outbound,
            Direction::Inbound => self.config.max_inbound,
        };
        // Inbound peers per network group are capped, so addresses in a few groups cannot fill
        // the inbound slots (or the refusal slots) of a well-known node such as a bootnode.
        let same_group = self
            .peers
            .values()
            .filter(|p| p.direction == Direction::Inbound && group(&p.addr) == group(&addr))
            .count();
        if !trusted && direction == Direction::Inbound && same_group >= MAX_INBOUND_PER_GROUP {
            return Err(DisconnectReason::TooManyPeers);
        }
        let mut refuse_at = None;
        if !trusted && count >= limit {
            let refusing = self.peers.values().filter(|p| p.refuse_at.is_some()).count();
            if direction == Direction::Outbound || refusing >= MAX_REFUSING {
                return Err(DisconnectReason::TooManyPeers);
            }
            refuse_at = Some(now + REFUSE_GRACE_MS);
        }
        let request_buckets = self
            .config
            .requests
            .iter()
            .map(|k| {
                (k.kind, Bucket::new(k.per_peer_per_second, k.per_peer_per_second.max(1), now))
            })
            .collect();
        let notice_buckets = self
            .config
            .notices
            .iter()
            .map(|k| {
                (k.kind, Bucket::new(k.per_peer_per_second, k.per_peer_per_second.max(1), now))
            })
            .collect();
        let gossip = self.config.gossip_per_second;
        self.peers.insert(
            conn,
            Peer {
                conn,
                key,
                id,
                addr,
                direction,
                status: None,
                connected_at: now,
                score: 0,
                score_at: now,
                last_recv: now,
                ping: None,
                last_ping: now,
                rtt_ms: 0,
                next_wire_id: 0,
                outstanding: BTreeMap::new(),
                inbound_requests: HashSet::new(),
                request_buckets,
                notice_buckets,
                gossip_bucket: Bucket::new(gossip, gossip.saturating_mul(2), now),
                ihave_bucket: Bucket::new(IHAVE_IDS_PER_SECOND, IHAVE_IDS_PER_SECOND, now),
                asked_peers: false,
                last_pex_answer: 0,
                trusted,
                refuse_at,
            },
        );
        self.by_id.insert(id, conn);
        if direction == Direction::Outbound {
            self.table.succeeded(&id, now);
        }
        let status = Status {
            version: PROTOCOL_VERSION,
            capabilities: self.config.capabilities,
            head_number: self.head.0,
            head_hash: self.head.1,
            listen_port: self.config.advertise.map_or(0, |a| a.port()),
        };
        self.send(conn, Message::Status(status));
        if let Some(record) = self.record.clone() {
            self.send(conn, Message::Peers(vec![record]));
        }
        if direction == Direction::Outbound {
            // Ask every peer we dial for its records once, so a node learns the network from its
            // first connection even when that peer is full and closes right after answering.
            self.peers.get_mut(&conn).expect("inserted").asked_peers = true;
            self.send(conn, Message::GetPeers(PEX_ANSWER_LIMIT as u16));
        }
        Ok(())
    }

    /// An outbound dial or its handshake failed.
    pub fn dial_failed(&mut self, key: &PublicKey) {
        let id = key.serialize();
        self.pending_dials.remove(&id);
        self.table.failed(&id);
    }

    /// A connection closed (either side).
    pub fn disconnected(&mut self, conn: ConnId) {
        let Some(peer) = self.peers.remove(&conn) else { return };
        self.by_id.remove(&peer.id);
        let ready = peer.ready();
        for (_, out) in peer.outstanding {
            self.requests.remove(&out.local);
            self.event(Event::Response {
                request: out.local,
                peer: peer.id,
                result: Err(RequestError::Disconnected),
            });
        }
        for seen in self.seen.values_mut() {
            seen.have.retain(|c| *c != conn);
            if seen.from == Some(conn) {
                seen.from = None;
            }
        }
        for want in self.wants.values_mut() {
            want.alternates.retain(|c| *c != conn);
        }
        if ready {
            self.event(Event::PeerGone { peer: peer.id });
        }
    }

    fn close(&mut self, conn: ConnId, reason: DisconnectReason) {
        if reason == DisconnectReason::ProtocolViolation {
            self.stats.violations += 1;
        }
        self.disconnected(conn);
        self.outputs.push_back(Output::Close { conn, reason });
    }

    fn penalize(&mut self, conn: ConnId, amount: i32, now: u64) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        if peer.trusted {
            return;
        }
        // Scores recover by one point per second toward zero.
        let recovered = (now.saturating_sub(peer.score_at) / 1000) as i32;
        peer.score = (peer.score + recovered).min(0) - amount;
        peer.score_at = now;
        if peer.score <= BAN_SCORE {
            let (id, ip) = (peer.id, peer.addr.ip());
            self.table.ban(&id, ip, now + BAN_MS);
            self.stats.bans += 1;
            self.close(conn, DisconnectReason::Misbehaving);
        }
    }

    /// The application reports a peer's bad response.
    pub fn report(&mut self, peer: &PeerId, penalty: Penalty, now: u64) {
        let Some(&conn) = self.by_id.get(peer) else { return };
        let amount = match penalty {
            Penalty::BadResponse => BAD_RESPONSE_PENALTY,
            Penalty::Useless => USELESS_PENALTY,
        };
        self.penalize(conn, amount, now);
    }

    /// A ready peer's score after recovery, for tests and measurements.
    pub fn score(&self, peer: &PeerId, now: u64) -> Option<i32> {
        let p = self.peers.get(self.by_id.get(peer)?)?;
        Some((p.score + (now.saturating_sub(p.score_at) / 1000) as i32).min(0))
    }

    /// A message arrived on a connection.
    pub fn received(&mut self, conn: ConnId, message: Message, now: u64) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        peer.last_recv = now;
        if peer.status.is_none() {
            match message {
                Message::Status(status) if status.version == PROTOCOL_VERSION => {
                    peer.status = Some(status.clone());
                    if peer.refuse_at.is_none() {
                        let id = peer.id;
                        self.event(Event::PeerReady { peer: id, status });
                    }
                }
                Message::Status(_) => self.close(conn, DisconnectReason::Incompatible),
                Message::Disconnect(_) => self.close(conn, DisconnectReason::Requested),
                _ => self.close(conn, DisconnectReason::ProtocolViolation),
            }
            return;
        }
        match message {
            Message::Status(_) => self.close(conn, DisconnectReason::ProtocolViolation),
            Message::Ping(nonce) => self.send(conn, Message::Pong(nonce)),
            Message::Pong(nonce) => {
                if let Some((sent_nonce, sent_at)) = peer.ping
                    && sent_nonce == nonce
                {
                    peer.rtt_ms = now.saturating_sub(sent_at);
                    peer.ping = None;
                }
            }
            Message::Disconnect(_) => self.close(conn, DisconnectReason::Requested),
            Message::GetPeers(n) => self.on_get_peers(conn, n, now),
            Message::Peers(records) => self.on_peers(conn, records),
            Message::Request { id, kind, body } => self.on_request(conn, id, kind, body, now),
            Message::Response { id, code, body } => self.on_response(conn, id, code, body, now),
            Message::Gossip { topic, data } => self.on_gossip(conn, topic, data, now),
            Message::IHave(ids) => self.on_ihave(conn, ids, now),
            Message::IWant(ids) => self.on_iwant(conn, ids),
            Message::IDontWant(ids) => self.on_idontwant(conn, ids, now),
            Message::Notify { kind, body } => self.on_notify(conn, kind, body, now),
        }
    }

    fn on_get_peers(&mut self, conn: ConnId, n: u16, now: u64) {
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if peer.last_pex_answer != 0 && now < peer.last_pex_answer + PEX_MIN_INTERVAL_MS {
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        peer.last_pex_answer = now;
        let refusing = peer.refuse_at.is_some();
        let n = usize::from(n).min(PEX_ANSWER_LIMIT);
        let records = self.table.sample(n, &mut self.rng);
        self.send(conn, Message::Peers(records));
        if refusing {
            self.close(conn, DisconnectReason::TooManyPeers);
        }
    }

    fn on_peers(&mut self, conn: ConnId, records: Vec<PeerRecord>) {
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        let source = peer.addr;
        let own_record = records.len() == 1 && records[0].key == peer.key;
        if !own_record && !peer.asked_peers {
            // Unsolicited lists are ignored, so a peer cannot push records at will.
            return;
        }
        if !own_record {
            peer.asked_peers = false;
        }
        for record in records {
            if record.key.serialize() == self.local_id || !record.verify(&self.network_id) {
                continue;
            }
            self.table.add_learned(record, &source);
        }
    }

    fn on_request(&mut self, conn: ConnId, id: u32, kind: u16, body: Vec<u8>, now: u64) {
        let Some(spec) = self.config.requests.iter().find(|k| k.kind == kind).copied() else {
            self.send(
                conn,
                Message::Response { id, code: ResponseCode::BadRequest, body: Vec::new() },
            );
            return;
        };
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if body.len() > spec.max_request || peer.inbound_requests.contains(&id) {
            self.close(conn, DisconnectReason::ProtocolViolation);
            return;
        }
        let allowed = peer.inbound_requests.len() < MAX_INBOUND_INFLIGHT_PER_PEER
            && peer.request_buckets.get_mut(&kind).expect("one bucket per kind").take(1.0, now);
        if !allowed {
            self.send(conn, Message::Response { id, code: ResponseCode::Busy, body: Vec::new() });
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        peer.inbound_requests.insert(id);
        let from = peer.id;
        self.event(Event::Request { peer: from, id, kind, body });
    }

    /// Answers a peer's request. A body longer than the kind's limit is refused with `Busy`.
    pub fn respond(
        &mut self,
        peer: &PeerId,
        id: u32,
        kind: u16,
        code: ResponseCode,
        body: Vec<u8>,
    ) {
        let Some(&conn) = self.by_id.get(peer) else { return };
        let Some(p) = self.peers.get_mut(&conn) else { return };
        if !p.inbound_requests.remove(&id) {
            return;
        }
        let max =
            self.config.requests.iter().find(|k| k.kind == kind).map_or(0, |k| k.max_response);
        let (code, body) = if code != ResponseCode::Ok {
            (code, Vec::new())
        } else if body.len() > max {
            (ResponseCode::Busy, Vec::new())
        } else {
            (code, body)
        };
        self.send(conn, Message::Response { id, code, body });
    }

    fn on_response(&mut self, conn: ConnId, id: u32, code: ResponseCode, body: Vec<u8>, now: u64) {
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        let Some(out) = peer.outstanding.remove(&id) else {
            // An answer to nothing we asked: the peer is broken or hostile.
            self.close(conn, DisconnectReason::ProtocolViolation);
            return;
        };
        let max =
            self.config.requests.iter().find(|k| k.kind == out.kind).map_or(0, |k| k.max_response);
        if body.len() > max {
            self.requests.remove(&out.local);
            let id = peer.id;
            self.event(Event::Response {
                request: out.local,
                peer: id,
                result: Err(RequestError::Refused(ResponseCode::BadRequest)),
            });
            self.close(conn, DisconnectReason::ProtocolViolation);
            return;
        }
        let from = peer.id;
        self.requests.remove(&out.local);
        let result =
            if code == ResponseCode::Ok { Ok(body) } else { Err(RequestError::Refused(code)) };
        let _ = now;
        self.event(Event::Response { request: out.local, peer: from, result });
    }

    /// Sends a request. The answer arrives as [`Event::Response`] with the returned id, also
    /// when no peer can take it.
    pub fn request(&mut self, kind: u16, body: Vec<u8>, target: Target, now: u64) -> RequestId {
        let local = self.next_request;
        self.next_request += 1;
        let spec = self
            .config
            .requests
            .iter()
            .find(|k| k.kind == kind)
            .copied()
            .expect("request kinds are configured");
        assert!(body.len() <= spec.max_request, "request body above its kind's limit");
        let candidates: Vec<ConnId> = self
            .peers
            .values()
            .filter(|p| {
                p.ready()
                    && p.status
                        .as_ref()
                        .is_some_and(|s| s.capabilities & spec.capability == spec.capability)
                    && p.outstanding.len() < MAX_INFLIGHT_PER_PEER
                    && match &target {
                        Target::Any => true,
                        Target::AnyExcept(except) => !except.contains(&p.id),
                        Target::Peer(id) => p.id == *id,
                    }
            })
            .map(|p| p.conn)
            .collect();
        // Fewest requests in flight, then best score, then lowest round trip; ties at random.
        let best = candidates.iter().copied().min_by_key(|c| {
            let p = &self.peers[c];
            (p.outstanding.len(), -p.score, p.rtt_ms, self.rng.next_u64())
        });
        let Some(conn) = best else {
            self.event(Event::Response {
                request: local,
                peer: [0; 33],
                result: Err(RequestError::NoPeer),
            });
            return local;
        };
        let peer = self.peers.get_mut(&conn).expect("candidate");
        let wire_id = peer.next_wire_id;
        peer.next_wire_id = peer.next_wire_id.wrapping_add(1);
        peer.outstanding
            .insert(wire_id, Outstanding { local, kind, deadline: now + spec.timeout_ms });
        self.requests.insert(local, (conn, wire_id));
        self.send(conn, Message::Request { id: wire_id, kind, body });
        local
    }

    fn on_notify(&mut self, conn: ConnId, kind: u16, body: Vec<u8>, now: u64) {
        let Some(spec) = self.config.notices.iter().find(|k| k.kind == kind).copied() else {
            return;
        };
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if body.len() > spec.max_len {
            self.close(conn, DisconnectReason::ProtocolViolation);
            return;
        }
        if !peer.notice_buckets.get_mut(&kind).expect("one bucket per kind").take(1.0, now) {
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        let from = peer.id;
        self.event(Event::Notify { peer: from, kind, body });
    }

    /// Sends a one-hop notice to one peer, or to every ready peer.
    pub fn notify(&mut self, peer: Option<&PeerId>, kind: u16, body: &[u8]) {
        let conns: Vec<ConnId> = match peer {
            Some(id) => self.by_id.get(id).copied().into_iter().collect(),
            None => self.peers.values().filter(|p| p.ready()).map(|p| p.conn).collect(),
        };
        for conn in conns {
            self.send(conn, Message::Notify { kind, body: body.to_vec() });
        }
    }

    fn remember(&mut self, id: MessageId, seen: Seen) {
        self.seen_order.push_back((seen.at, id));
        self.seen.insert(id, seen);
        while self.seen.len() > MAX_SEEN {
            let Some((_, oldest)) = self.seen_order.pop_front() else { break };
            if let Some(old) = self.seen.remove(&oldest)
                && old.state == SeenState::Pending
            {
                self.pending_validation -= 1;
            }
        }
    }

    fn on_gossip(&mut self, conn: ConnId, topic: u8, data: Vec<u8>, now: u64) {
        self.stats.gossip_received += 1;
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if data.len() > self.config.max_gossip_len {
            self.close(conn, DisconnectReason::ProtocolViolation);
            return;
        }
        if !peer.gossip_bucket.take(1.0, now) {
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        let from = peer.id;
        let id = message_id(topic, &data);
        self.wants.remove(&id);
        if let Some(seen) = self.seen.get_mut(&id) {
            self.stats.gossip_duplicates += 1;
            if !seen.have.contains(&conn) {
                seen.have.push(conn);
            }
            return;
        }
        if self.pending_validation >= MAX_PENDING_VALIDATION {
            // The application is behind: drop it unseen, a peer that has it will offer it again.
            return;
        }
        self.pending_validation += 1;
        if data.len() >= IDONTWANT_MIN_LEN {
            let others: Vec<ConnId> = self
                .peers
                .values()
                .filter(|p| p.ready() && p.conn != conn)
                .map(|p| p.conn)
                .collect();
            for other in others {
                self.send(other, Message::IDontWant(vec![id]));
            }
        }
        self.remember(
            id,
            Seen {
                state: SeenState::Pending,
                at: now,
                topic,
                data: Some(data.clone()),
                from: Some(conn),
                have: vec![conn],
            },
        );
        self.event(Event::Gossip { id, topic, data, from });
    }

    /// The application's verdict on a gossip message from [`Event::Gossip`].
    pub fn validated(&mut self, id: &MessageId, validation: Validation, now: u64) {
        let Some(seen) = self.seen.get_mut(id) else { return };
        if seen.state != SeenState::Pending {
            return;
        }
        self.pending_validation -= 1;
        match validation {
            Validation::Accept => {
                seen.state = SeenState::Accepted;
                seen.at = now;
                self.forward(*id);
            }
            Validation::Ignore => {
                seen.state = SeenState::Done;
                seen.data = None;
            }
            Validation::Reject => {
                seen.state = SeenState::Done;
                seen.data = None;
                if let Some(from) = seen.from {
                    self.penalize(from, REJECT_PENALTY, now);
                }
            }
        }
    }

    /// Publishes a message of our own: no validation round, forwarded at once.
    pub fn publish(&mut self, topic: u8, data: Vec<u8>, now: u64) -> MessageId {
        let id = message_id(topic, &data);
        if self.seen.contains_key(&id) {
            return id;
        }
        self.remember(
            id,
            Seen {
                state: SeenState::Accepted,
                at: now,
                topic,
                data: Some(data),
                from: None,
                have: Vec::new(),
            },
        );
        self.forward(id);
        id
    }

    /// Sends an accepted message in full to up to `gossip_fanout` peers that lack it, and its id
    /// to the rest.
    fn forward(&mut self, id: MessageId) {
        let seen = &self.seen[&id];
        let mut targets: Vec<ConnId> = self
            .peers
            .values()
            .filter(|p| p.ready() && !seen.have.contains(&p.conn))
            .map(|p| p.conn)
            .collect();
        targets.sort_unstable();
        // Fisher-Yates, deterministic under the node's seed.
        for i in (1..targets.len()).rev() {
            let j = self.rng.below(i as u64 + 1) as usize;
            targets.swap(i, j);
        }
        let (topic, data) =
            (seen.topic, seen.data.clone().expect("accepted messages keep their data"));
        let fanout = if data.len() >= self.config.large_gossip {
            self.config.gossip_fanout_large
        } else {
            self.config.gossip_fanout
        };
        let fanout = fanout.min(targets.len());
        for (i, conn) in targets.into_iter().enumerate() {
            if i < fanout {
                self.stats.gossip_sent += 1;
                self.send(conn, Message::Gossip { topic, data: data.clone() });
                self.seen.get_mut(&id).expect("present").have.push(conn);
            } else {
                self.stats.ihave_sent += 1;
                self.send(conn, Message::IHave(vec![id]));
            }
        }
    }

    fn on_ihave(&mut self, conn: ConnId, ids: Vec<MessageId>, now: u64) {
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if !peer.ihave_bucket.take(ids.len() as f64, now) {
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        let mut ask = Vec::new();
        for id in ids {
            if let Some(seen) = self.seen.get_mut(&id) {
                if !seen.have.contains(&conn) {
                    seen.have.push(conn);
                }
                continue;
            }
            let room = self.wants.len() < MAX_WANTS;
            match self.wants.get_mut(&id) {
                Some(want) => {
                    if want.asked != conn
                        && !want.alternates.contains(&conn)
                        && want.alternates.len() < MAX_WANT_ALTERNATES
                    {
                        want.alternates.push(conn);
                    }
                }
                None if room => {
                    self.wants.insert(
                        id,
                        Want {
                            asked: conn,
                            deadline: now + WANT_TIMEOUT_MS,
                            alternates: Vec::new(),
                        },
                    );
                    ask.push(id);
                }
                None => {}
            }
        }
        if !ask.is_empty() {
            self.stats.iwant_sent += ask.len() as u64;
            self.send(conn, Message::IWant(ask));
        }
    }

    fn on_idontwant(&mut self, conn: ConnId, ids: Vec<MessageId>, now: u64) {
        let peer = self.peers.get_mut(&conn).expect("caller checked");
        if !peer.ihave_bucket.take(ids.len() as f64, now) {
            self.penalize(conn, RATE_PENALTY, now);
            return;
        }
        for id in ids {
            // Only for messages we hold: a peer cannot grow our state with ids we never saw.
            if let Some(seen) = self.seen.get_mut(&id)
                && !seen.have.contains(&conn)
            {
                seen.have.push(conn);
            }
        }
    }

    fn on_iwant(&mut self, conn: ConnId, ids: Vec<MessageId>) {
        for id in ids {
            let Some(seen) = self.seen.get_mut(&id) else { continue };
            // Each accepted message goes to a peer at most once, so asking again amplifies nothing.
            if seen.state != SeenState::Accepted || seen.have.contains(&conn) {
                continue;
            }
            let Some(data) = seen.data.clone() else { continue };
            seen.have.push(conn);
            let topic = seen.topic;
            self.stats.gossip_sent += 1;
            self.send(conn, Message::Gossip { topic, data });
        }
    }

    /// Advances timers: request and want deadlines, pings, Status and validation timeouts, cache
    /// expiry, dialing and peer exchange.
    pub fn tick(&mut self, now: u64) {
        // Requests past their deadline.
        let mut expired = Vec::new();
        for peer in self.peers.values_mut() {
            peer.outstanding.retain(|_, out| {
                if now >= out.deadline {
                    expired.push((peer.conn, peer.id, out.local));
                    false
                } else {
                    true
                }
            });
        }
        for (conn, id, local) in expired {
            self.requests.remove(&local);
            self.event(Event::Response {
                request: local,
                peer: id,
                result: Err(RequestError::Timeout),
            });
            self.penalize(conn, TIMEOUT_PENALTY, now);
        }

        // Pings, and peers that went silent or never sent Status.
        let mut silent = Vec::new();
        let mut pings = Vec::new();
        let mut refused = Vec::new();
        for peer in self.peers.values_mut() {
            if peer.refuse_at.is_some_and(|at| now >= at) {
                refused.push(peer.conn);
                continue;
            }
            if peer.status.is_none() {
                if now >= peer.connected_at + STATUS_TIMEOUT_MS {
                    silent.push(peer.conn);
                }
                continue;
            }
            if let Some((_, sent)) = peer.ping
                && now >= sent + PING_TIMEOUT_MS
                && now >= peer.last_recv + PING_TIMEOUT_MS
            {
                silent.push(peer.conn);
                continue;
            }
            if peer.ping.is_none() && now >= peer.last_ping + PING_INTERVAL_MS {
                let nonce = self.rng.next_u64();
                peer.ping = Some((nonce, now));
                peer.last_ping = now;
                pings.push((peer.conn, nonce));
            }
        }
        for conn in silent {
            self.close(conn, DisconnectReason::Timeout);
        }
        for conn in refused {
            self.close(conn, DisconnectReason::TooManyPeers);
        }
        for (conn, nonce) in pings {
            self.send(conn, Message::Ping(nonce));
        }

        // Wants: ask the next peer that announced the id.
        let mut retry = Vec::new();
        self.wants.retain(|id, want| {
            if now < want.deadline {
                return true;
            }
            if want.alternates.is_empty() {
                return false;
            }
            want.asked = want.alternates.remove(0);
            want.deadline = now + WANT_TIMEOUT_MS;
            retry.push((want.asked, *id));
            true
        });
        for (conn, id) in retry {
            self.stats.iwant_sent += 1;
            self.send(conn, Message::IWant(vec![id]));
        }

        // Expire seen entries, cached data and stuck validations.
        while let Some(&(at, id)) = self.seen_order.front() {
            if now < at + SEEN_TTL_MS {
                break;
            }
            self.seen_order.pop_front();
            if let Some(old) = self.seen.remove(&id)
                && old.state == SeenState::Pending
            {
                self.pending_validation -= 1;
            }
        }
        for seen in self.seen.values_mut() {
            if seen.state == SeenState::Accepted && now >= seen.at + CACHE_TTL_MS {
                seen.state = SeenState::Done;
                seen.data = None;
            }
            if seen.state == SeenState::Pending && now >= seen.at + VALIDATION_TIMEOUT_MS {
                seen.state = SeenState::Done;
                seen.data = None;
                self.pending_validation -= 1;
            }
        }
        self.table.expire_bans(now);

        // Dial toward the outbound target.
        if now >= self.last_dial + DIAL_INTERVAL_MS {
            self.last_dial = now;
            self.pending_dials.retain(|_, (at, _)| now < *at + 30_000);
            let outbound =
                self.peers.values().filter(|p| p.direction == Direction::Outbound).count();
            let mut want =
                self.config.max_outbound.saturating_sub(outbound + self.pending_dials.len());
            want = want.min(MAX_PENDING_DIALS.saturating_sub(self.pending_dials.len()));
            for _ in 0..want {
                let groups: HashSet<u64> = self
                    .peers
                    .values()
                    .filter(|p| p.direction == Direction::Outbound)
                    .map(|p| group(&p.addr))
                    .chain(self.pending_dials.values().map(|(_, g)| *g))
                    .collect();
                let connected = |id: &PeerId| {
                    self.by_id.contains_key(id) || self.pending_dials.contains_key(id)
                };
                let Some((key, addr)) =
                    self.table
                        .select_dial(now, &mut self.rng, &connected, &|g| groups.contains(&g))
                else {
                    break;
                };
                let id = key.serialize();
                self.table.attempted(&id, now);
                self.pending_dials.insert(id, (now, group(&addr)));
                self.outputs.push_back(Output::Dial { key, addr });
            }
        }

        // Peer exchange with one random outbound peer.
        if now >= self.last_pex + PEX_INTERVAL_MS {
            self.last_pex = now;
            // Ask an outbound peer (one we chose); with none, any peer, since a node that knows
            // only its inbound peers must still learn whom to dial. Answers are bucketed by
            // their source's group either way (table.rs).
            let mut candidates: Vec<ConnId> = self
                .peers
                .values()
                .filter(|p| p.ready() && p.direction == Direction::Outbound)
                .map(|p| p.conn)
                .collect();
            if candidates.is_empty() {
                candidates = self.peers.values().filter(|p| p.ready()).map(|p| p.conn).collect();
            }
            if !candidates.is_empty() {
                let conn = candidates[self.rng.below(candidates.len() as u64) as usize];
                self.peers.get_mut(&conn).expect("listed").asked_peers = true;
                self.send(conn, Message::GetPeers(PEX_ANSWER_LIMIT as u16));
            }
        }
    }

    /// Asks one ready peer for its peer records now (used at startup and by tests).
    pub fn request_peers(&mut self, peer: &PeerId) {
        let Some(&conn) = self.by_id.get(peer) else { return };
        self.peers.get_mut(&conn).expect("indexed").asked_peers = true;
        self.send(conn, Message::GetPeers(PEX_ANSWER_LIMIT as u16));
    }

    /// A protocol error found below this layer (bad chunk or undecodable message): close the
    /// connection as a violation.
    pub fn violation(&mut self, conn: ConnId) {
        if self.peers.contains_key(&conn) {
            self.close(conn, DisconnectReason::ProtocolViolation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(i: u8) -> SecretKey {
        SecretKey::from_byte_array([i; 32]).unwrap()
    }

    const NET: Network = Network { chain_id: 8453, genesis: [1; 32], layer: Network::CONSENSUS };

    fn node(i: u8) -> Node {
        let mut config = Config::new(NET, secret(i), u64::from(i));
        config.requests.push(RequestKind {
            kind: 1,
            capability: 1,
            max_request: 64,
            max_response: 1024,
            per_peer_per_second: 2,
            timeout_ms: 1000,
        });
        config.capabilities = 1;
        Node::new(config, 0)
    }

    /// Connects `a` (outbound) to `b` (inbound) as conn ids `ca` and `cb`, delivers messages
    /// between them until both are quiet, and returns the events each produced.
    struct Pair {
        a: Node,
        b: Node,
    }

    const CA: ConnId = 1;
    const CB: ConnId = 2;

    impl Pair {
        fn new() -> Self {
            let mut pair = Self { a: node(1), b: node(2) };
            let (ka, kb) = (pair.a_key(), pair.b_key());
            pair.a
                .connected(CA, kb, "10.0.0.2:1".parse().unwrap(), Direction::Outbound, 0)
                .unwrap();
            pair.b.connected(CB, ka, "10.0.0.1:1".parse().unwrap(), Direction::Inbound, 0).unwrap();
            pair.pump(0);
            pair
        }
        fn a_key(&self) -> PublicKey {
            PublicKey::from_secret_key(SECP256K1, &secret(1))
        }
        fn b_key(&self) -> PublicKey {
            PublicKey::from_secret_key(SECP256K1, &secret(2))
        }
        fn pump(&mut self, now: u64) -> (Vec<Event>, Vec<Event>, Vec<Output>) {
            let (mut ea, mut eb, mut closes) = (Vec::new(), Vec::new(), Vec::new());
            loop {
                let mut moved = false;
                while let Some(out) = self.a.poll() {
                    moved = true;
                    match out {
                        Output::Send { message, .. } => self.b.received(CB, message, now),
                        Output::Event(e) => ea.push(e),
                        other => closes.push(other),
                    }
                }
                while let Some(out) = self.b.poll() {
                    moved = true;
                    match out {
                        Output::Send { message, .. } => self.a.received(CA, message, now),
                        Output::Event(e) => eb.push(e),
                        other => closes.push(other),
                    }
                }
                if !moved {
                    return (ea, eb, closes);
                }
            }
        }
    }

    #[test]
    fn status_then_request_response() {
        let mut p = Pair::new();
        assert_eq!(p.a.peers().len(), 1);
        let req = p.a.request(1, vec![9], Target::Any, 10);
        let (_, eb, _) = p.pump(10);
        let Event::Request { peer, id, kind, .. } = &eb[0] else { panic!("{eb:?}") };
        p.b.respond(peer, *id, *kind, ResponseCode::Ok, vec![1, 2, 3]);
        let (ea, _, _) = p.pump(11);
        assert!(ea.contains(&Event::Response {
            request: req,
            peer: p.b_key().serialize(),
            result: Ok(vec![1, 2, 3])
        }));
    }

    #[test]
    fn rate_limited_requests_get_busy_and_timeouts_fire() {
        let mut p = Pair::new();
        let ids: Vec<_> = (0..4).map(|_| p.a.request(1, vec![], Target::Any, 10)).collect();
        let (ea, eb, _) = p.pump(10);
        // Burst of 2: two reach b's application, two are answered Busy.
        assert_eq!(eb.iter().filter(|e| matches!(e, Event::Request { .. })).count(), 2);
        assert_eq!(
            ea.iter()
                .filter(|e| matches!(
                    e,
                    Event::Response { result: Err(RequestError::Refused(ResponseCode::Busy)), .. }
                ))
                .count(),
            2
        );
        p.a.tick(2000);
        let (ea, _, _) = p.pump(2000);
        let timed_out: Vec<_> = ea
            .iter()
            .filter_map(|e| match e {
                Event::Response { request, result: Err(RequestError::Timeout), .. } => {
                    Some(*request)
                }
                _ => None,
            })
            .collect();
        assert_eq!(timed_out.len(), 2);
        assert!(timed_out.iter().all(|r| ids.contains(r)));
    }

    #[test]
    fn unrequested_response_and_message_before_status_close() {
        let mut p = Pair::new();
        p.b.received(CB, Message::Response { id: 77, code: ResponseCode::Ok, body: vec![] }, 5);
        let outs: Vec<_> = std::iter::from_fn(|| p.b.poll()).collect();
        assert!(
            outs.contains(&Output::Close { conn: CB, reason: DisconnectReason::ProtocolViolation }),
            "{outs:?}"
        );
        let mut fresh = node(3);
        fresh
            .connected(9, p.a_key(), "10.0.0.9:1".parse().unwrap(), Direction::Inbound, 0)
            .unwrap();
        while fresh.poll().is_some() {}
        fresh.received(9, Message::Ping(1), 1);
        assert!(matches!(
            fresh.poll(),
            Some(Output::Close { reason: DisconnectReason::ProtocolViolation, .. })
        ));
    }

    #[test]
    fn gossip_is_forwarded_only_after_accept_and_rejects_ban() {
        let mut p = Pair::new();
        let id = p.a.publish(3, vec![1, 2, 3], 10);
        let (_, eb, _) = p.pump(10);
        assert!(eb.contains(&Event::Gossip {
            id,
            topic: 3,
            data: vec![1, 2, 3],
            from: p.a_key().serialize()
        }));
        // Duplicate from the same peer is not delivered again.
        p.b.received(CB, Message::Gossip { topic: 3, data: vec![1, 2, 3] }, 11);
        assert!(p.b.poll().is_none());
        // Four rejected messages in a row ban the sender (25 each, ban at -100).
        for i in 0..4u8 {
            p.b.received(CB, Message::Gossip { topic: 3, data: vec![i, 9] }, 20);
            let gid = message_id(3, &[i, 9]);
            while p.b.poll().is_some() {}
            p.b.validated(&gid, Validation::Reject, 20);
        }
        assert!(p.b.peers().is_empty());
        assert_eq!(p.b.stats().bans, 1);
        assert_eq!(
            p.b.connected(5, p.a_key(), "10.0.0.1:1".parse().unwrap(), Direction::Inbound, 30),
            Err(DisconnectReason::Misbehaving)
        );
    }

    #[test]
    fn ihave_iwant_fetches_once() {
        let mut p = Pair::new();
        let data = vec![7u8; 100];
        let id = message_id(1, &data);
        // With fanout 0, b announces the id instead of sending the message.
        p.b.config.gossip_fanout = 0;
        p.b.publish(1, data.clone(), 0);
        assert_eq!(p.b.poll(), Some(Output::Send { conn: CB, message: Message::IHave(vec![id]) }));
        p.a.received(CA, Message::IHave(vec![id]), 1);
        assert_eq!(p.a.poll(), Some(Output::Send { conn: CA, message: Message::IWant(vec![id]) }));
        p.b.received(CB, Message::IWant(vec![id, id]), 1);
        let sent: Vec<_> = std::iter::from_fn(|| p.b.poll()).collect();
        assert_eq!(
            sent,
            vec![Output::Send { conn: CB, message: Message::Gossip { topic: 1, data } }]
        );
        p.b.received(CB, Message::IWant(vec![id]), 2);
        assert!(p.b.poll().is_none());
    }

    #[test]
    fn prologue_layout_is_fixed() {
        let p = NET.prologue();
        assert_eq!(p.len(), 47);
        assert_eq!(&p[..6], b"tp2p/1");
        assert_eq!(&p[6..14], &8453u64.to_be_bytes());
        assert_eq!(p[46], Network::CONSENSUS);
    }
}
