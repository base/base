//! Node Discovery v5.1 with a configurable protocol id, the way base-reth runs it: protocol id
//! `basev0` instead of `discv5`, so Base execution nodes form their own discovery network, and
//! the fork id under the ENR key `opel`.
//!
//! Spec: <https://github.com/ethereum/devp2p/blob/master/discv5/discv5-wire.md>. Implemented:
//! the three packet kinds, the WHOAREYOU handshake with session keys, PING/PONG, FINDNODE/NODES,
//! TALKREQ (answered empty), a 256-bucket routing table, and random-target lookups that feed
//! RLPx dial candidates. One thread owns the socket and all state.

use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use ring::hkdf;
use secp256k1::{PublicKey, SecretKey};

use crate::{
    crypto::{
        aes128_ctr, aes128_gcm_decrypt, aes128_gcm_encrypt, peer_id, public_key, random_array,
        random_secret_key, sha256, sign_compact, verify_compact,
    },
    enr::Enr,
    eth::encode_list,
    forkid::ForkId,
    network::NodeRecord,
};

/// base-reth's discv5 protocol id (`BASE_V0_PROTOCOL_VERSION` in `crates/execution/node`).
pub const BASE_PROTOCOL_ID: [u8; 6] = *b"basev0";
/// The standard discv5 protocol id, used by the spec's test vectors.
pub const DISCV5_PROTOCOL_ID: [u8; 6] = *b"discv5";

const VERSION: u16 = 1;
const MAX_PACKET: usize = 1280;
const MIN_PACKET: usize = 63;
const STATIC_HEADER: usize = 23;
const FLAG_MESSAGE: u8 = 0;
const FLAG_WHOAREYOU: u8 = 1;
const FLAG_HANDSHAKE: u8 = 2;
const PING: u8 = 0x01;
const PONG: u8 = 0x02;
const FINDNODE: u8 = 0x03;
const NODES: u8 = 0x04;
const TALKREQ: u8 = 0x05;
const TALKRESP: u8 = 0x06;
const BUCKET_SIZE: usize = 16;
const LOOKUP_PARALLELISM: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SESSIONS: usize = 1024;
const MAX_FOUND: usize = 1024;
/// How long a verified node waits to replace a full bucket's oldest entry, which is probed in
/// the meantime (sigp discv5's k-bucket pending timeout).
const PENDING_TIMEOUT: Duration = Duration::from_secs(60);
/// Agreeing PONG votes from distinct peers needed to change the advertised socket (sigp
/// discv5's `enr_peer_update_min` default, which base-reth keeps).
const ENR_PEER_UPDATE_MIN: usize = 10;
/// How long a PONG vote counts (sigp discv5's `vote_duration` default).
const VOTE_DURATION: Duration = Duration::from_secs(120);
/// The winning socket's lead over the runner-up, as a fraction of its votes (sigp discv5's
/// `CLEAR_MAJORITY_PERCENTAGE`).
const CLEAR_MAJORITY_PERCENTAGE: f64 = 0.3;

/// Discovery configuration.
#[derive(Debug, Clone)]
pub struct Discv5Config {
    /// The node key.
    pub secret_key: SecretKey,
    /// UDP listen address.
    pub listen_addr: SocketAddr,
    /// The RLPx port to advertise (0 for none).
    pub tcp_port: u16,
    /// The IP to advertise; when `None`, the advertised UDP socket follows PONG votes.
    pub external_ip: Option<IpAddr>,
    /// The fork id to advertise under `opel`.
    pub fork_id: ForkId,
    /// Nodes to bootstrap from.
    pub bootnodes: Vec<NodeRecord>,
}

/// A node found by discovery with an RLPx endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundNode {
    /// The dialable record.
    pub record: NodeRecord,
    /// The fork id in its ENR, if any.
    pub fork_id: Option<ForkId>,
}

/// Session keys with one node.
#[derive(Debug, Clone, Copy)]
struct Session {
    write_key: [u8; 16],
    read_key: [u8; 16],
}

/// A node we can talk to: its key and address, and its record once known.
#[derive(Debug, Clone)]
struct Contact {
    public_key: PublicKey,
    addr: SocketAddr,
    enr: Option<Enr>,
}

/// A verified node waiting to replace the oldest entry of its full bucket, which is pinged and
/// keeps its place if it answers before `replace_at`.
#[derive(Debug, Clone)]
struct PendingEntry {
    id: B256,
    contact: Contact,
    oldest: B256,
    replace_at: Instant,
}

/// The sockets peers report for this node in PONGs: one vote per peer, each counted for
/// [`VOTE_DURATION`], as sigp discv5's `IpVote` keeps them.
#[derive(Debug, Default)]
struct IpVotes {
    votes: HashMap<B256, (SocketAddrV4, Instant)>,
}

impl IpVotes {
    fn insert(&mut self, node: B256, socket: SocketAddrV4, now: Instant) {
        self.votes.insert(node, (socket, now + VOTE_DURATION));
    }

    /// Drops expired votes and returns the socket with at least [`ENR_PEER_UPDATE_MIN`] votes
    /// and a clear lead over every other socket, if there is one.
    fn majority(&mut self, now: Instant) -> Option<SocketAddrV4> {
        self.votes.retain(|_, (_, expires)| *expires > now);
        let mut counts: HashMap<SocketAddrV4, usize> = HashMap::new();
        for (socket, _) in self.votes.values() {
            *counts.entry(*socket).or_default() += 1;
        }
        let mut best: Option<(SocketAddrV4, usize)> = None;
        let mut runner_up = 0;
        for (socket, count) in counts {
            match best {
                Some((_, best_count)) if count <= best_count => runner_up = runner_up.max(count),
                _ => {
                    runner_up = runner_up.max(best.map_or(0, |(_, best_count)| best_count));
                    best = Some((socket, count));
                }
            }
        }
        let (socket, count) = best?;
        let threshold = ((count as f64) * (1.0 - CLEAR_MAJORITY_PERCENTAGE)).round() as usize;
        (count >= ENR_PEER_UPDATE_MIN && runner_up < threshold).then_some(socket)
    }
}

/// A request we sent, kept until answered, challenged, or timed out.
#[derive(Debug, Clone)]
struct Outgoing {
    node: B256,
    plaintext: Vec<u8>,
    sent: Instant,
}

/// A WHOAREYOU we sent, waiting for the handshake.
#[derive(Debug, Clone)]
struct Challenge {
    data: Vec<u8>,
    addr: SocketAddr,
    sent: Instant,
}

struct State {
    local_enr: Enr,
    contacts: HashMap<B256, Contact>,
    sessions: HashMap<B256, Session>,
    outgoing: HashMap<[u8; 12], Outgoing>,
    challenges: HashMap<B256, Challenge>,
    buckets: Vec<Vec<B256>>,
    /// At most one pending entry per bucket, keyed by bucket index.
    pending: HashMap<usize, PendingEntry>,
    lookup_target: B256,
    lookup_asked: HashSet<B256>,
    lookup_rounds: u32,
    found: Vec<FoundNode>,
    next_request_id: u64,
    /// `None` when the IP is configured: the advertised socket then never changes.
    ip_votes: Option<IpVotes>,
}

struct Inner {
    socket: UdpSocket,
    key: SecretKey,
    local_id: B256,
    protocol_id: [u8; 6],
    config: Discv5Config,
    state: Mutex<State>,
    shutdown: AtomicBool,
}

/// A running discovery service.
#[derive(Clone)]
pub struct Discv5 {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Discv5 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discv5").field("local_id", &self.inner.local_id).finish()
    }
}

/// The node id of a public key: keccak256 of the uncompressed key.
fn node_id(key: &PublicKey) -> B256 {
    keccak256(peer_id(key))
}

/// The log2 distance between two node ids (0 if equal, else 1..=256).
fn log2_distance(a: &B256, b: &B256) -> u32 {
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        let diff = x ^ y;
        if diff != 0 {
            return 256 - (i as u32 * 8 + diff.leading_zeros());
        }
    }
    0
}

/// ECDH as discv5 defines it: the compressed shared point, 33 bytes.
fn ecdh_compressed(public: &PublicKey, secret: &SecretKey) -> [u8; 33] {
    let point = secp256k1::ecdh::shared_secret_point(public, secret);
    let mut out = [0u8; 33];
    out[0] = 0x02 | (point[63] & 1);
    out[1..].copy_from_slice(&point[..32]);
    out
}

/// Derives (initiator-key, recipient-key) for a handshake.
fn derive_keys(
    secret: &[u8; 33],
    challenge: &[u8],
    id_a: &B256,
    id_b: &B256,
) -> ([u8; 16], [u8; 16]) {
    let info: [&[u8]; 3] = [b"discovery v5 key agreement", id_a.as_slice(), id_b.as_slice()];
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, challenge).extract(secret);
    let mut keys = [0u8; 32];
    prk.expand(&info, hkdf::HKDF_SHA256)
        .and_then(|okm| okm.fill(&mut keys))
        .expect("32 bytes is a valid HKDF-SHA-256 output length");
    (keys[..16].try_into().unwrap(), keys[16..].try_into().unwrap())
}

/// The digest signed by `id-signature`.
fn id_signature_digest(challenge: &[u8], ephemeral: &[u8], dest: &B256) -> B256 {
    B256::from(sha256(&[b"discovery v5 identity proof", challenge, ephemeral, dest.as_slice()]))
}

/// A decoded packet header.
struct Packet<'a> {
    masking_iv: [u8; 16],
    /// `static-header || authdata`, unmasked.
    header: Vec<u8>,
    flag: u8,
    nonce: [u8; 12],
    authdata_start: usize,
    message: &'a [u8],
}

impl Packet<'_> {
    fn authdata(&self) -> &[u8] {
        &self.header[self.authdata_start..]
    }

    /// `masking-iv || header`, the AES-GCM associated data and the WHOAREYOU challenge data.
    fn iv_and_header(&self) -> Vec<u8> {
        let mut out = self.masking_iv.to_vec();
        out.extend_from_slice(&self.header);
        out
    }
}

fn decode_packet<'a>(protocol_id: &[u8; 6], local_id: &B256, data: &'a [u8]) -> Option<Packet<'a>> {
    if data.len() < MIN_PACKET || data.len() > MAX_PACKET {
        return None;
    }
    let masking_iv: [u8; 16] = data[..16].try_into().unwrap();
    let key: [u8; 16] = local_id[..16].try_into().unwrap();
    // Unmask everything after the IV, then keep the static header and the authdata.
    let mut header = data[16..].to_vec();
    aes128_ctr(&key, &masking_iv, &mut header);
    if &header[..6] != protocol_id || u16::from_be_bytes([header[6], header[7]]) != VERSION {
        return None;
    }
    let authdata_size = u16::from_be_bytes([header[21], header[22]]) as usize;
    let header_end = 16 + STATIC_HEADER + authdata_size;
    if header_end > data.len() {
        return None;
    }
    header.truncate(header_end - 16);
    Some(Packet {
        masking_iv,
        flag: header[8],
        nonce: header[9..21].try_into().unwrap(),
        header,
        authdata_start: STATIC_HEADER,
        message: &data[header_end..],
    })
}

/// Builds `masking-iv || masked header`, returning it with the unmasked `iv || header` (the
/// associated data for the message).
fn encode_header(
    protocol_id: &[u8; 6],
    dest: &B256,
    flag: u8,
    nonce: &[u8; 12],
    authdata: &[u8],
    masking_iv: [u8; 16],
) -> (Vec<u8>, Vec<u8>) {
    let mut packet = masking_iv.to_vec();
    packet.extend_from_slice(protocol_id);
    packet.extend_from_slice(&VERSION.to_be_bytes());
    packet.push(flag);
    packet.extend_from_slice(nonce);
    packet.extend_from_slice(&(authdata.len() as u16).to_be_bytes());
    packet.extend_from_slice(authdata);
    let ad = packet.clone();
    let key: [u8; 16] = dest[..16].try_into().unwrap();
    aes128_ctr(&key, &masking_iv, &mut packet[16..]);
    (packet, ad)
}

/// The message kind and request id of a request or response plaintext.
fn request_kind_and_id(plaintext: &[u8]) -> Option<(u8, &[u8])> {
    let (&kind, mut rest) = plaintext.split_first()?;
    let mut items = RlpHeader::decode_bytes(&mut rest, true).ok()?;
    Some((kind, RlpHeader::decode_bytes(&mut items, false).ok()?))
}

fn encode_message(kind: u8, fields: &[&dyn Encodable]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend(encode_list(fields));
    out
}

impl Discv5 {
    /// Binds the socket and starts the discovery thread, with Base's protocol id.
    pub fn start(config: Discv5Config) -> io::Result<Self> {
        let socket = UdpSocket::bind(config.listen_addr)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let udp_port = socket.local_addr()?.port();
        let ip = match config.external_ip {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => match config.listen_addr.ip() {
                IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
                _ => None,
            },
        };
        let local_enr =
            Enr::build(&config.secret_key, 1, ip, config.tcp_port, udp_port, Some(config.fork_id));
        let key = config.secret_key;
        let local_id = node_id(&PublicKey::from_secret_key_global(&key));
        let mut contacts = HashMap::new();
        for boot in &config.bootnodes {
            if let Some(public_key) = public_key(&boot.id) {
                let addr = SocketAddr::new(boot.addr.ip(), boot.udp_port);
                contacts.insert(node_id(&public_key), Contact { public_key, addr, enr: None });
            }
        }
        let inner = Arc::new(Inner {
            socket,
            key,
            local_id,
            protocol_id: BASE_PROTOCOL_ID,
            state: Mutex::new(State {
                ip_votes: ip.is_none().then(IpVotes::default),
                local_enr,
                contacts,
                sessions: HashMap::new(),
                outgoing: HashMap::new(),
                challenges: HashMap::new(),
                buckets: vec![Vec::new(); 257],
                pending: HashMap::new(),
                lookup_target: local_id,
                lookup_asked: HashSet::new(),
                lookup_rounds: 0,
                found: Vec::new(),
                next_request_id: 1,
            }),
            config,
            shutdown: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        thread::Builder::new().name("tiger-discv5".into()).spawn(move || run(thread_inner))?;
        Ok(Self { inner })
    }

    /// Takes the dialable nodes found since the last call.
    pub fn take_found(&self) -> Vec<FoundNode> {
        std::mem::take(&mut self.inner.state.lock().unwrap().found)
    }

    /// The local record in text form.
    pub fn local_enr_text(&self) -> String {
        self.inner.state.lock().unwrap().local_enr.to_text()
    }

    /// The UDP address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.socket.local_addr().expect("bound socket")
    }

    /// Stops the discovery thread.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
    }
}

fn run(inner: Arc<Inner>) {
    let mut buf = [0u8; MAX_PACKET + 1];
    // Ask every bootnode for its record and its neighbours.
    {
        let mut state = inner.state.lock().unwrap();
        let boots: Vec<B256> = state.contacts.keys().copied().collect();
        for node in boots {
            inner.find_node(&mut state, node, &[0, 256, 255, 254]);
        }
    }
    let mut last_tick = Instant::now();
    while !inner.shutdown.load(Ordering::Relaxed) {
        match inner.socket.recv_from(&mut buf) {
            Ok((len, from)) if len <= MAX_PACKET => inner.on_packet(&buf[..len], from),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
        if last_tick.elapsed() > Duration::from_millis(500) {
            last_tick = Instant::now();
            inner.tick();
        }
    }
}

impl Inner {
    fn send_to(&self, packet: &[u8], addr: SocketAddr) {
        debug_assert!(packet.len() <= MAX_PACKET);
        let _ = self.socket.send_to(packet, addr);
    }

    fn request_id(state: &mut State) -> Bytes {
        let id = state.next_request_id;
        state.next_request_id += 1;
        Bytes::from(id.to_be_bytes().to_vec())
    }

    /// Builds a packet of kind `flag` to `dest` with a fresh nonce: the masked header, then the
    /// message sealed with `key`, or without a key 20 random bytes, which provoke a WHOAREYOU.
    fn seal(
        &self,
        dest: &B256,
        flag: u8,
        authdata: &[u8],
        key: Option<&[u8; 16]>,
        plaintext: &[u8],
    ) -> ([u8; 12], Vec<u8>) {
        let nonce: [u8; 12] = random_array();
        let (mut packet, ad) =
            encode_header(&self.protocol_id, dest, flag, &nonce, authdata, random_array());
        match key {
            Some(key) => packet.extend(aes128_gcm_encrypt(key, &nonce, plaintext, &ad)),
            None => packet.extend_from_slice(&random_array::<20>()),
        }
        (nonce, packet)
    }

    /// Sends a request to a known contact, through its session or by starting a handshake.
    fn send_request(&self, state: &mut State, node: B256, plaintext: Vec<u8>) {
        let Some(contact) = state.contacts.get(&node) else { return };
        let addr = contact.addr;
        let key = state.sessions.get(&node).map(|session| session.write_key);
        let (nonce, packet) =
            self.seal(&node, FLAG_MESSAGE, self.local_id.as_slice(), key.as_ref(), &plaintext);
        self.send_to(&packet, addr);
        state.outgoing.insert(nonce, Outgoing { node, plaintext, sent: Instant::now() });
    }

    fn find_node(&self, state: &mut State, node: B256, distances: &[u16]) {
        let id = Self::request_id(state);
        let message = encode_message(FINDNODE, &[&id, &distances.to_vec()]);
        self.send_request(state, node, message);
    }

    fn send_response(&self, state: &State, node: B256, addr: SocketAddr, plaintext: &[u8]) {
        let Some(session) = state.sessions.get(&node) else { return };
        let key = Some(&session.write_key);
        let (_, packet) = self.seal(&node, FLAG_MESSAGE, self.local_id.as_slice(), key, plaintext);
        self.send_to(&packet, addr);
    }

    fn on_packet(&self, data: &[u8], from: SocketAddr) {
        let Some(packet) = decode_packet(&self.protocol_id, &self.local_id, data) else { return };
        let mut state = self.state.lock().unwrap();
        match packet.flag {
            FLAG_MESSAGE => self.on_message_packet(&mut state, &packet, from),
            FLAG_WHOAREYOU => self.on_whoareyou(&mut state, &packet, from),
            FLAG_HANDSHAKE => self.on_handshake(&mut state, &packet, from),
            _ => {}
        }
    }

    fn on_message_packet(&self, state: &mut State, packet: &Packet<'_>, from: SocketAddr) {
        if packet.authdata().len() != 32 {
            return;
        }
        let src = B256::from_slice(packet.authdata());
        let plaintext = state.sessions.get(&src).and_then(|session| {
            aes128_gcm_decrypt(
                &session.read_key,
                &packet.nonce,
                packet.message,
                &packet.iv_and_header(),
            )
        });
        match plaintext {
            Some(plaintext) => self.on_message(state, src, from, &plaintext),
            None => self.send_whoareyou(state, src, from, &packet.nonce),
        }
    }

    fn send_whoareyou(&self, state: &mut State, src: B256, from: SocketAddr, nonce: &[u8; 12]) {
        if let Some(existing) = state.challenges.get(&src)
            && existing.sent.elapsed() < Duration::from_millis(500)
        {
            return;
        }
        let known_seq = state.contacts.get(&src).and_then(|c| c.enr.as_ref()).map_or(0, |e| e.seq);
        let mut authdata = random_array::<16>().to_vec();
        authdata.extend_from_slice(&known_seq.to_be_bytes());
        let masking_iv = random_array();
        let (packet, challenge) =
            encode_header(&self.protocol_id, &src, FLAG_WHOAREYOU, nonce, &authdata, masking_iv);
        self.send_to(&packet, from);
        if state.challenges.len() >= MAX_SESSIONS {
            state.challenges.retain(|_, c| c.sent.elapsed() < REQUEST_TIMEOUT);
        }
        state
            .challenges
            .insert(src, Challenge { data: challenge, addr: from, sent: Instant::now() });
    }

    fn on_whoareyou(&self, state: &mut State, packet: &Packet<'_>, from: SocketAddr) {
        if packet.authdata().len() != 24 || !packet.message.is_empty() {
            return;
        }
        let Some(request) = state.outgoing.remove(&packet.nonce) else { return };
        let Some(contact) = state.contacts.get(&request.node).cloned() else { return };
        if contact.addr != from {
            return;
        }
        let enr_seq = u64::from_be_bytes(packet.authdata()[16..24].try_into().unwrap());
        let challenge = packet.iv_and_header();
        let ephemeral = random_secret_key();
        let ephemeral_public = PublicKey::from_secret_key_global(&ephemeral).serialize();
        let secret = ecdh_compressed(&contact.public_key, &ephemeral);
        let (write_key, read_key) = derive_keys(&secret, &challenge, &self.local_id, &request.node);
        let signature = sign_compact(
            id_signature_digest(&challenge, &ephemeral_public, &request.node),
            &self.key,
        );
        let mut authdata = self.local_id.to_vec();
        authdata.push(64);
        authdata.push(33);
        authdata.extend_from_slice(&signature);
        authdata.extend_from_slice(&ephemeral_public);
        if enr_seq < state.local_enr.seq {
            authdata.extend_from_slice(&state.local_enr.encode());
        }
        let key = Some(&write_key);
        let (nonce, out) =
            self.seal(&request.node, FLAG_HANDSHAKE, &authdata, key, &request.plaintext);
        self.send_to(&out, from);
        state.sessions.insert(request.node, Session { write_key, read_key });
        let node = request.node;
        let request_kind = request_kind_and_id(&request.plaintext).map(|(kind, _)| kind);
        state.outgoing.insert(nonce, Outgoing { sent: Instant::now(), ..request });
        // Like sigp discv5, ping a node we just connected to, so its PONG can vote on our
        // advertised socket.
        if state.ip_votes.is_some() && request_kind != Some(PING) {
            self.ping(state, node);
        }
    }

    fn on_handshake(&self, state: &mut State, packet: &Packet<'_>, from: SocketAddr) {
        let auth = packet.authdata();
        if auth.len() < 34 {
            return;
        }
        let src = B256::from_slice(&auth[..32]);
        let (sig_size, key_size) = (auth[32] as usize, auth[33] as usize);
        if sig_size != 64 || key_size != 33 || auth.len() < 34 + 64 + 33 {
            return;
        }
        let signature = &auth[34..98];
        let ephemeral = &auth[98..131];
        let record = &auth[131..];
        let Some(challenge) = state.challenges.get(&src) else { return };
        if challenge.addr != from {
            return;
        }
        let challenge = challenge.data.clone();
        let enr = if record.is_empty() {
            None
        } else {
            let Ok(enr) = Enr::decode(&mut &record[..]) else { return };
            if enr.node_id() != src {
                return;
            }
            Some(enr)
        };
        let public_key = match &enr {
            Some(enr) => *enr.public_key(),
            None => match state.contacts.get(&src) {
                Some(contact) => contact.public_key,
                None => return,
            },
        };
        let digest = id_signature_digest(&challenge, ephemeral, &self.local_id);
        if !verify_compact(digest, signature, &public_key) {
            // Keep the challenge for the real node's answer, as sigp discv5 does.
            return;
        }
        state.challenges.remove(&src);
        let Ok(ephemeral_key) = PublicKey::from_slice(ephemeral) else { return };
        let secret = ecdh_compressed(&ephemeral_key, &self.key);
        let (initiator_key, recipient_key) = derive_keys(&secret, &challenge, &src, &self.local_id);
        // The id signature and the ephemeral key check out, so the record may enter the table
        // now, at its own UDP endpoint, before the message is decrypted, as in sigp discv5. Like
        // sigp, a record with no UDP endpoint, or one other than the packet's source, gets no
        // table entry.
        if let Some(enr) = enr
            && let Some(udp) = enr.udp_socket()
            && udp == from
        {
            self.add_contact(state, enr, udp, true);
        }
        let Some(plaintext) = aes128_gcm_decrypt(
            &initiator_key,
            &packet.nonce,
            packet.message,
            &packet.iv_and_header(),
        ) else {
            // sigp drops the new session and challenges the node again, with the sequence
            // number of its record if the record is now in the table.
            state.sessions.remove(&src);
            self.send_whoareyou(state, src, from, &packet.nonce);
            return;
        };
        if state.sessions.len() >= MAX_SESSIONS {
            state.sessions.clear();
        }
        state.sessions.insert(src, Session { write_key: recipient_key, read_key: initiator_key });
        self.on_message(state, src, from, &plaintext);
    }

    fn on_message(&self, state: &mut State, src: B256, from: SocketAddr, plaintext: &[u8]) {
        self.on_alive(state, src);
        let Some((&kind, rest)) = plaintext.split_first() else { return };
        let mut buf = rest;
        let Ok(mut items) = RlpHeader::decode_bytes(&mut buf, true) else { return };
        let Ok(request_id) = Bytes::decode(&mut items) else { return };
        if request_id.len() > 8 {
            return;
        }
        match kind {
            PING => {
                let ip: Vec<u8> = match from.ip() {
                    IpAddr::V4(ip) => ip.octets().to_vec(),
                    IpAddr::V6(ip) => ip.octets().to_vec(),
                };
                let seq = state.local_enr.seq;
                let pong = encode_message(PONG, &[&request_id, &seq, &ip.as_slice(), &from.port()]);
                self.send_response(state, src, from, &pong);
            }
            PONG => {
                // Only a PONG answering one of our PINGs to `src` counts, as in sigp discv5.
                let answered = state.outgoing.iter().find(|(_, o)| {
                    o.node == src && request_kind_and_id(&o.plaintext) == Some((PING, &request_id))
                });
                let Some(nonce) = answered.map(|(nonce, _)| *nonce) else { return };
                state.outgoing.remove(&nonce);
                let _seq = u64::decode(&mut items);
                let (Ok(ip), Ok(port)) = (Bytes::decode(&mut items), u16::decode(&mut items))
                else {
                    return;
                };
                if let Ok(octets) = <[u8; 4]>::try_from(ip.as_ref())
                    && port != 0
                {
                    self.on_ip_vote(state, src, SocketAddrV4::new(Ipv4Addr::from(octets), port));
                }
            }
            FINDNODE => {
                let Ok(distances) = Vec::<u16>::decode(&mut items) else { return };
                let mut records: Vec<Vec<u8>> = Vec::new();
                for distance in distances.iter().take(3) {
                    if *distance == 0 {
                        records.push(state.local_enr.encode());
                    } else if let Some(bucket) = state.buckets.get(*distance as usize) {
                        for node in bucket.iter().take(BUCKET_SIZE) {
                            if let Some(enr) = state.contacts.get(node).and_then(|c| c.enr.as_ref())
                            {
                                records.push(enr.encode());
                            }
                        }
                    }
                }
                records.truncate(BUCKET_SIZE);
                // Three records per NODES message keeps each packet under 1280 bytes.
                let chunks: Vec<&[Vec<u8>]> =
                    if records.is_empty() { vec![&[]] } else { records.chunks(3).collect() };
                let total = chunks.len() as u64;
                for chunk in chunks {
                    let list = crate::eth::encode_list_of_raw(chunk);
                    let message =
                        encode_message(NODES, &[&request_id, &total, &crate::eth::RawRlp(&list)]);
                    self.send_response(state, src, from, &message);
                }
            }
            NODES => {
                state.outgoing.retain(|_, o| o.node != src);
                let _total = u64::decode(&mut items);
                let Ok(mut list) = RlpHeader::decode_bytes(&mut items, true) else { return };
                let mut count = 0;
                while !list.is_empty() && count < BUCKET_SIZE {
                    count += 1;
                    let Ok(enr) = Enr::decode(&mut list) else { return };
                    let Some(udp) = enr.udp_socket() else { continue };
                    self.add_contact(state, enr, udp, false);
                }
            }
            TALKREQ => {
                let message = encode_message(TALKRESP, &[&request_id, &&b""[..]]);
                self.send_response(state, src, from, &message);
            }
            _ => {}
        }
    }

    fn ping(&self, state: &mut State, node: B256) {
        let request_id = Self::request_id(state);
        let ping = encode_message(PING, &[&request_id, &state.local_enr.seq]);
        self.send_request(state, node, ping);
    }

    /// Counts `socket`, reported by `node` in a PONG to our PING, towards our advertised UDP
    /// socket. As in sigp discv5, only peers in the routing table vote, and the record changes
    /// once one socket holds a clear majority of [`ENR_PEER_UPDATE_MIN`] or more live votes.
    fn on_ip_vote(&self, state: &mut State, node: B256, socket: SocketAddrV4) {
        let distance = log2_distance(&self.local_id, &node) as usize;
        if !state.buckets[distance].contains(&node) {
            return;
        }
        let now = Instant::now();
        let Some(votes) = state.ip_votes.as_mut() else { return };
        votes.insert(node, socket, now);
        let Some(majority) = votes.majority(now) else { return };
        if state.local_enr.udp_socket() == Some(SocketAddr::V4(majority)) {
            return;
        }
        let seq = state.local_enr.seq + 1;
        state.local_enr = Enr::build(
            &self.key,
            seq,
            Some(*majority.ip()),
            self.config.tcp_port,
            majority.port(),
            Some(self.config.fork_id),
        );
    }

    /// Adds or updates a node in the routing table. `verified` is true only for a record whose
    /// node completed a handshake with us; records relayed in NODES answers are not.
    fn add_contact(&self, state: &mut State, enr: Enr, udp: SocketAddr, verified: bool) {
        let id = enr.node_id();
        if id == self.local_id {
            return;
        }
        if let Some(tcp) = enr.tcp_socket()
            && state.found.len() < MAX_FOUND
        {
            state.found.push(FoundNode {
                record: NodeRecord { id: enr.peer_id(), addr: tcp, udp_port: udp.port() },
                fork_id: enr.fork_id(),
            });
        }
        let distance = log2_distance(&self.local_id, &id) as usize;
        let contact = Contact { public_key: *enr.public_key(), addr: udp, enr: Some(enr) };
        let bucket = &mut state.buckets[distance];
        if bucket.contains(&id) || bucket.len() < BUCKET_SIZE {
            if !bucket.contains(&id) {
                bucket.push(id);
            }
            state.contacts.insert(id, contact);
            return;
        }
        // Full bucket, as in sigp discv5: relayed records are dropped, and a verified node waits
        // as the bucket's pending entry while the oldest entry is pinged.
        if !verified || state.pending.contains_key(&distance) {
            return;
        }
        let oldest = bucket[0];
        let replace_at = Instant::now() + PENDING_TIMEOUT;
        state.pending.insert(distance, PendingEntry { id, contact, oldest, replace_at });
        self.ping(state, oldest);
    }

    /// Records that `node` answered us: if it is the entry a pending node waits to replace, it
    /// keeps its place, moves to the bucket's tail, and the pending node is dropped.
    fn on_alive(&self, state: &mut State, node: B256) {
        let distance = log2_distance(&self.local_id, &node) as usize;
        if state.pending.get(&distance).is_none_or(|pending| pending.oldest != node) {
            return;
        }
        state.pending.remove(&distance);
        let bucket = &mut state.buckets[distance];
        if let Some(position) = bucket.iter().position(|id| *id == node) {
            bucket.remove(position);
            bucket.push(node);
        }
    }

    /// Replaces the probed entries that did not answer in time with their pending nodes.
    fn apply_pending(state: &mut State) {
        let now = Instant::now();
        let due: Vec<usize> =
            state.pending.iter().filter(|(_, p)| p.replace_at <= now).map(|(d, _)| *d).collect();
        for distance in due {
            let pending = state.pending.remove(&distance).expect("due pending entry");
            let bucket = &mut state.buckets[distance];
            if let Some(position) = bucket.iter().position(|id| *id == pending.oldest) {
                bucket.remove(position);
                state.contacts.remove(&pending.oldest);
                state.sessions.remove(&pending.oldest);
            }
            if bucket.len() < BUCKET_SIZE && !bucket.contains(&pending.id) {
                bucket.push(pending.id);
                state.contacts.insert(pending.id, pending.contact);
            }
            assert!(bucket.len() <= BUCKET_SIZE);
        }
    }

    /// Periodic work: expire requests and challenges, and advance the lookup.
    fn tick(&self) {
        let mut state = self.state.lock().unwrap();
        state.outgoing.retain(|_, o| o.sent.elapsed() < REQUEST_TIMEOUT);
        state.challenges.retain(|_, c| c.sent.elapsed() < REQUEST_TIMEOUT);
        Self::apply_pending(&mut state);
        if state.lookup_rounds >= 8 {
            state.lookup_target = B256::from(random_array::<32>());
            state.lookup_asked.clear();
            state.lookup_rounds = 0;
        }
        state.lookup_rounds += 1;
        let target = state.lookup_target;
        let mut candidates: Vec<B256> =
            state.contacts.keys().filter(|id| !state.lookup_asked.contains(*id)).copied().collect();
        candidates.sort_by_key(|id| *id ^ target);
        for node in candidates.into_iter().take(LOOKUP_PARALLELISM) {
            state.lookup_asked.insert(node);
            let distance = log2_distance(&node, &target).max(1) as u16;
            let distances: Vec<u16> =
                [distance, distance.saturating_add(1).min(256), distance.saturating_sub(1).max(1)]
                    .to_vec();
            self.find_node(&mut state, node, &distances);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, hex};

    fn key(text: &str) -> SecretKey {
        SecretKey::from_byte_array(hex::decode(text).unwrap().try_into().unwrap()).unwrap()
    }

    const NODE_A: &str = "eef77acb6c6a6eebc5b363a475ac583ec7eccdb42b6481424c60f59aa326547f";
    const NODE_B: &str = "66fb62bfbd66b9177a138c1e5cddbe4f7c30c343e94e68df8769459cb1cde628";

    #[test]
    fn decodes_the_spec_ping_packet() {
        let b = key(NODE_B);
        let b_id = node_id(&PublicKey::from_secret_key_global(&b));
        assert_eq!(b_id, b256!("bbbb9d047f0488c0b5a93c1c3f2d8bafc7c8ff337024a55434a0d0555de64db9"));
        let data = hex::decode(
            "00000000000000000000000000000000088b3d4342774649325f313964a39e55\
             ea96c005ad52be8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d3\
             4c4f53245d08dab84102ed931f66d1492acb308fa1c6715b9d139b81acbdcc",
        )
        .unwrap();
        let packet = decode_packet(&DISCV5_PROTOCOL_ID, &b_id, &data).unwrap();
        assert_eq!(packet.flag, FLAG_MESSAGE);
        assert_eq!(packet.nonce, [0xff; 12]);
        let a_id = node_id(&PublicKey::from_secret_key_global(&key(NODE_A)));
        assert_eq!(packet.authdata(), a_id.as_slice());
        let plaintext =
            aes128_gcm_decrypt(&[0u8; 16], &packet.nonce, packet.message, &packet.iv_and_header())
                .unwrap();
        // PING [req-id 0x00000001, enr-seq 2]
        assert_eq!(hex::encode(plaintext), "01c6840000000102");
        // The same bytes under Base's protocol id do not decode.
        assert!(decode_packet(&BASE_PROTOCOL_ID, &b_id, &data).is_none());
    }

    #[test]
    fn decodes_the_spec_whoareyou_and_handshake_packets() {
        let b = key(NODE_B);
        let b_id = node_id(&PublicKey::from_secret_key_global(&b));
        let whoareyou = hex::decode(
            "00000000000000000000000000000000088b3d434277464933a1ccc59f5967ad\
             1d6035f15e528627dde75cd68292f9e6c27d6b66c8100a873fcbaed4e16b8d",
        )
        .unwrap();
        // The spec addresses every vector packet to node B.
        let a = key(NODE_A);
        let a_id = node_id(&PublicKey::from_secret_key_global(&a));
        let packet = decode_packet(&DISCV5_PROTOCOL_ID, &b_id, &whoareyou).unwrap();
        assert_eq!(packet.flag, FLAG_WHOAREYOU);
        assert_eq!(
            hex::encode(packet.iv_and_header()),
            "000000000000000000000000000000006469736376350001010102030405060708090a0b0c00180102030405060708090a0b0c0d0e0f100000000000000000"
        );

        // Handshake packet (flag 2, no record): B verifies A's id signature and derives the
        // read key given in the vectors.
        let handshake = hex::decode(
            "00000000000000000000000000000000088b3d4342774649305f313964a39e55\
             ea96c005ad521d8c7560413a7008f16c9e6d2f43bbea8814a546b7409ce783d3\
             4c4f53245d08da4bb252012b2cba3f4f374a90a75cff91f142fa9be3e0a5f3ef\
             268ccb9065aeecfd67a999e7fdc137e062b2ec4a0eb92947f0d9a74bfbf44dfb\
             a776b21301f8b65efd5796706adff216ab862a9186875f9494150c4ae06fa4d1\
             f0396c93f215fa4ef524f1eadf5f0f4126b79336671cbcf7a885b1f8bd2a5d83\
             9cf8",
        )
        .unwrap();
        let challenge = hex::decode("000000000000000000000000000000006469736376350001010102030405060708090a0b0c00180102030405060708090a0b0c0d0e0f100000000000000001").unwrap();
        let packet = decode_packet(&DISCV5_PROTOCOL_ID, &b_id, &handshake).unwrap();
        assert_eq!(packet.flag, FLAG_HANDSHAKE);
        let auth = packet.authdata();
        assert_eq!(&auth[..32], a_id.as_slice());
        let ephemeral = &auth[98..131];
        let digest = id_signature_digest(&challenge, ephemeral, &b_id);
        assert!(verify_compact(digest, &auth[34..98], &PublicKey::from_secret_key_global(&a)));
        let secret = ecdh_compressed(&PublicKey::from_slice(ephemeral).unwrap(), &b);
        let (initiator_key, _) = derive_keys(&secret, &challenge, &a_id, &b_id);
        assert_eq!(hex::encode(initiator_key), "4f9fac6de7567d1e3b1241dffe90f662");
        let plaintext = aes128_gcm_decrypt(
            &initiator_key,
            &packet.nonce,
            packet.message,
            &packet.iv_and_header(),
        )
        .unwrap();
        assert_eq!(hex::encode(plaintext), "01c6840000000101");
    }

    #[test]
    fn crypto_primitives_match_spec_vectors() {
        let ephemeral = key("fb757dc581730490a1d7a00deea65e9b1936924caaea8f44d476014856b68736");
        let dest = PublicKey::from_slice(
            &hex::decode("0317931e6e0840220642f230037d285d122bc59063221ef3226b1f403ddc69ca91")
                .unwrap(),
        )
        .unwrap();
        let challenge = hex::decode("000000000000000000000000000000006469736376350001010102030405060708090a0b0c00180102030405060708090a0b0c0d0e0f100000000000000000").unwrap();
        let id_a = b256!("aaaa8419e9f49d0083561b48287df592939a8d19947d8c0ef88f2a4856a69fbb");
        let id_b = b256!("bbbb9d047f0488c0b5a93c1c3f2d8bafc7c8ff337024a55434a0d0555de64db9");
        let (initiator, recipient) =
            derive_keys(&ecdh_compressed(&dest, &ephemeral), &challenge, &id_a, &id_b);
        assert_eq!(hex::encode(initiator), "dccc82d81bd610f4f76d3ebe97a40571");
        assert_eq!(hex::encode(recipient), "ac74bb8773749920b0d3a8881c173ec5");

        let ephemeral_public =
            hex::decode("039961e4c2356d61bedb83052c115d311acb3a96f5777296dcf297351130266231")
                .unwrap();
        let signature =
            sign_compact(id_signature_digest(&challenge, &ephemeral_public, &id_b), &ephemeral);
        assert_eq!(
            hex::encode(signature),
            "94852a1e2318c4e5e9d422c98eaf19d1d90d876b29cd06ca7cb7546d0fff7b484fe86c09a064fe72bdbef73ba8e9c34df0cd2b53e9d65528c2c7f336d5dfc6e6"
        );

        let ct = aes128_gcm_encrypt(
            &hex::decode("9f2d77db7004bf8a1a85107ac686990b").unwrap().try_into().unwrap(),
            &hex::decode("27b5af763c446acd2749fe8e").unwrap().try_into().unwrap(),
            &hex::decode("01c20101").unwrap(),
            &hex::decode("93a7400fa0d6a694ebc24d5cf570f65d04215b6ac00757875e3f3a5f42107903")
                .unwrap(),
        );
        assert_eq!(hex::encode(ct), "a5d12a2d94b8ccb3ba55558229867dc13bfa3648");
    }

    /// A discovery service without its thread, so tests drive it packet by packet.
    fn test_inner() -> Inner {
        let key = random_secret_key();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let listen_addr = socket.local_addr().unwrap();
        let local_id = node_id(&PublicKey::from_secret_key_global(&key));
        let fork_id = ForkId { hash: [1, 2, 3, 4], next: 0 };
        let local_enr =
            Enr::build(&key, 1, Some(Ipv4Addr::LOCALHOST), 0, listen_addr.port(), None);
        Inner {
            socket,
            key,
            local_id,
            protocol_id: BASE_PROTOCOL_ID,
            config: Discv5Config {
                secret_key: key,
                listen_addr,
                tcp_port: 0,
                external_ip: None,
                fork_id,
                bootnodes: vec![],
            },
            state: Mutex::new(State {
                local_enr,
                contacts: HashMap::new(),
                sessions: HashMap::new(),
                outgoing: HashMap::new(),
                challenges: HashMap::new(),
                buckets: vec![Vec::new(); 257],
                pending: HashMap::new(),
                lookup_target: local_id,
                lookup_asked: HashSet::new(),
                lookup_rounds: 0,
                found: Vec::new(),
                next_request_id: 1,
                ip_votes: None,
            }),
            shutdown: AtomicBool::new(false),
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tamper {
        None,
        IdSignature,
        EphemeralKey,
        Message,
    }

    /// A handshake packet from `remote` answering the WHOAREYOU `inner` sent it, carrying
    /// `record` and a PING, with one part optionally corrupted.
    fn handshake_packet(
        inner: &Inner,
        remote: &SecretKey,
        record: &[u8],
        tamper: Tamper,
    ) -> Vec<u8> {
        let remote_id = node_id(&PublicKey::from_secret_key_global(remote));
        let challenge = inner.state.lock().unwrap().challenges[&remote_id].data.clone();
        let ephemeral = random_secret_key();
        let mut ephemeral_public = PublicKey::from_secret_key_global(&ephemeral).serialize();
        if tamper == Tamper::EphemeralKey {
            // 33 bytes that are not a compressed secp256k1 point.
            ephemeral_public = [0x05; 33];
        }
        let local_public = PublicKey::from_secret_key_global(&inner.key);
        let secret = ecdh_compressed(&local_public, &ephemeral);
        let (write_key, _) = derive_keys(&secret, &challenge, &remote_id, &inner.local_id);
        let digest = id_signature_digest(&challenge, &ephemeral_public, &inner.local_id);
        let mut signature = sign_compact(digest, remote);
        if tamper == Tamper::IdSignature {
            signature[10] ^= 1;
        }
        let mut authdata = remote_id.to_vec();
        authdata.extend_from_slice(&[64, 33]);
        authdata.extend_from_slice(&signature);
        authdata.extend_from_slice(&ephemeral_public);
        authdata.extend_from_slice(record);
        let nonce: [u8; 12] = random_array();
        let (mut packet, ad) = encode_header(
            &BASE_PROTOCOL_ID,
            &inner.local_id,
            FLAG_HANDSHAKE,
            &nonce,
            &authdata,
            random_array(),
        );
        let ping = encode_message(PING, &[&Bytes::from_static(&[1]), &1u64]);
        let mut sealed = aes128_gcm_encrypt(&write_key, &nonce, &ping, &ad);
        if tamper == Tamper::Message {
            sealed[0] ^= 1;
        }
        packet.extend(sealed);
        packet
    }

    #[test]
    fn handshake_record_enters_the_table_only_after_verification() {
        // The remote's UDP socket; it receives tiger's WHOAREYOU and PONG and is never read.
        let remote_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let from = remote_socket.local_addr().unwrap();
        let run = |record_udp: u16, tamper: Tamper| {
            let inner = test_inner();
            let remote = random_secret_key();
            let remote_id = node_id(&PublicKey::from_secret_key_global(&remote));
            let ip = (record_udp != 0).then_some(Ipv4Addr::LOCALHOST);
            let record = Enr::build(&remote, 1, ip, 30303, record_udp, None).encode();
            inner.send_whoareyou(&mut inner.state.lock().unwrap(), remote_id, from, &[7; 12]);
            inner.on_packet(&handshake_packet(&inner, &remote, &record, tamper), from);
            let state = inner.state.lock().unwrap();
            let in_table = state.buckets.iter().any(|bucket| bucket.contains(&remote_id));
            let contact = state.contacts.get(&remote_id).map(|contact| contact.addr);
            assert_eq!(in_table, contact.is_some());
            (contact, state.sessions.contains_key(&remote_id))
        };
        // A valid, correctly signed record with a flipped id signature byte or a garbage
        // ephemeral key: no session and no table entry.
        for tamper in [Tamper::IdSignature, Tamper::EphemeralKey] {
            assert_eq!(run(from.port(), tamper), (None, false));
        }
        // An undecryptable message after a valid id signature and key: the record enters the
        // table, but there is no session.
        assert_eq!(run(from.port(), Tamper::Message), (Some(from), false));
        // A verified handshake stores the record's own endpoint.
        assert_eq!(run(from.port(), Tamper::None), (Some(from), true));
        // A record with another UDP endpoint, or none, gets a session but no table entry.
        assert_eq!(run(from.port() ^ 1, Tamper::None), (None, true));
        assert_eq!(run(0, Tamper::None), (None, true));
    }

    #[test]
    fn undecryptable_handshake_message_is_challenged_again() {
        let inner = test_inner();
        let remote_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        remote_socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let from = remote_socket.local_addr().unwrap();
        let remote = random_secret_key();
        let remote_id = node_id(&PublicKey::from_secret_key_global(&remote));
        let record = Enr::build(&remote, 3, Some(Ipv4Addr::LOCALHOST), 0, from.port(), None);
        // Reads the next WHOAREYOU sent to the remote: (its nonce, the enr-seq it carries).
        let mut buf = [0u8; MAX_PACKET];
        let mut next_whoareyou = || {
            let len = remote_socket.recv(&mut buf).unwrap();
            let packet = decode_packet(&BASE_PROTOCOL_ID, &remote_id, &buf[..len]).unwrap();
            assert_eq!(packet.flag, FLAG_WHOAREYOU);
            (packet.nonce, u64::from_be_bytes(packet.authdata()[16..24].try_into().unwrap()))
        };
        inner.send_whoareyou(&mut inner.state.lock().unwrap(), remote_id, from, &[7; 12]);
        assert_eq!(next_whoareyou(), ([7; 12], 0));
        let handshake = handshake_packet(&inner, &remote, &record.encode(), Tamper::Message);
        let nonce = decode_packet(&BASE_PROTOCOL_ID, &inner.local_id, &handshake).unwrap().nonce;
        inner.on_packet(&handshake, from);
        // The re-challenge answers the handshake's nonce and names the record's seq.
        assert_eq!(next_whoareyou(), (nonce, 3));
        let state = inner.state.lock().unwrap();
        assert_eq!(state.challenges[&remote_id].addr, from);
        assert!(!state.sessions.contains_key(&remote_id));
    }

    #[test]
    fn full_bucket_probes_its_oldest_entry_before_eviction() {
        let inner = test_inner();
        // Records at the farthest distance, 256 (half of all node ids).
        let mut records = Vec::new();
        while records.len() < BUCKET_SIZE + 2 {
            let key = random_secret_key();
            let id = node_id(&PublicKey::from_secret_key_global(&key));
            if log2_distance(&inner.local_id, &id) == 256 {
                let port = 40_000 + records.len() as u16;
                records.push(Enr::build(&key, 1, Some(Ipv4Addr::LOCALHOST), 0, port, None));
            }
        }
        let add = |enr: &Enr, verified: bool| {
            let mut state = inner.state.lock().unwrap();
            inner.add_contact(&mut state, enr.clone(), enr.udp_socket().unwrap(), verified);
        };
        let bucket = || inner.state.lock().unwrap().buckets[256].clone();
        for enr in &records[..BUCKET_SIZE] {
            add(enr, false);
        }
        let full = bucket();
        assert_eq!(full.len(), BUCKET_SIZE);
        // A relayed record never displaces an entry.
        add(&records[BUCKET_SIZE], false);
        assert_eq!(bucket(), full);
        // A verified node waits while the oldest entry is pinged; an answer keeps the oldest
        // (moved to the tail) and drops the waiting node.
        let verified = &records[BUCKET_SIZE + 1];
        add(verified, true);
        assert_eq!(bucket(), full);
        {
            let mut state = inner.state.lock().unwrap();
            assert!(state.outgoing.values().any(|request| request.node == full[0]));
            let pong = encode_message(PONG, &[&Bytes::from_static(&[1]), &1u64]);
            inner.on_message(&mut state, full[0], "127.0.0.1:1".parse().unwrap(), &pong);
            assert!(state.pending.is_empty());
        }
        let mut rotated = full[1..].to_vec();
        rotated.push(full[0]);
        assert_eq!(bucket(), rotated);
        // When the probed entry stays silent past the timeout, the verified node replaces it.
        add(verified, true);
        inner.state.lock().unwrap().pending.get_mut(&256).unwrap().replace_at = Instant::now();
        inner.tick();
        let mut replaced = rotated[1..].to_vec();
        replaced.push(verified.node_id());
        assert_eq!(bucket(), replaced);
        let state = inner.state.lock().unwrap();
        assert!(!state.contacts.contains_key(&full[1]));
        assert!(state.contacts.contains_key(&verified.node_id()));
    }

    #[test]
    fn advertised_socket_changes_only_after_enough_agreeing_solicited_pongs() {
        let inner = test_inner();
        inner.state.lock().unwrap().ip_votes = Some(IpVotes::default());
        let reported = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 30303);
        let pong = |request_id: &[u8]| {
            let ip = reported.ip().octets();
            let request_id = Bytes::copy_from_slice(request_id);
            encode_message(PONG, &[&request_id, &1u64, &&ip[..], &reported.port()])
        };
        let from: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let peers: Vec<B256> = (0..ENR_PEER_UPDATE_MIN as u16)
            .map(|i| {
                let key = random_secret_key();
                let enr = Enr::build(&key, 1, Some(Ipv4Addr::LOCALHOST), 0, 40_000 + i, None);
                let (id, udp) = (enr.node_id(), enr.udp_socket().unwrap());
                inner.add_contact(&mut inner.state.lock().unwrap(), enr, udp, false);
                id
            })
            .collect();
        let advertised = || {
            let state = inner.state.lock().unwrap();
            (state.local_enr.seq, state.local_enr.udp_socket())
        };
        let before = advertised();
        // A PONG with req-id 0xdeadbeef that answers no PING casts no vote.
        {
            let mut state = inner.state.lock().unwrap();
            inner.on_message(&mut state, peers[0], from, &pong(&[0xde, 0xad, 0xbe, 0xef]));
            assert!(state.ip_votes.as_ref().unwrap().votes.is_empty());
        }
        assert_eq!(advertised(), before);
        let solicited = |peer: B256| {
            let mut state = inner.state.lock().unwrap();
            inner.ping(&mut state, peer);
            let ping = state.outgoing.values().find(|o| o.node == peer).unwrap();
            let request_id = request_kind_and_id(&ping.plaintext).unwrap().1.to_vec();
            inner.on_message(&mut state, peer, from, &pong(&request_id));
        };
        // Nine agreeing votes, and a repeat from one of those peers, are not enough.
        for peer in &peers[..ENR_PEER_UPDATE_MIN - 1] {
            solicited(*peer);
        }
        solicited(peers[0]);
        assert_eq!(advertised(), before);
        // The tenth distinct peer moves the record to the reported socket.
        solicited(peers[ENR_PEER_UPDATE_MIN - 1]);
        assert_eq!(advertised(), (before.0 + 1, Some(SocketAddr::V4(reported))));
    }

    #[test]
    fn ip_vote_majority_needs_a_clear_lead_and_live_votes() {
        let a = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 30303);
        let b = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 30304);
        let now = Instant::now();
        let votes = |for_a: usize, for_b: usize| {
            let mut votes = IpVotes::default();
            for i in 0..for_a + for_b {
                let socket = if i < for_a { a } else { b };
                votes.insert(B256::from(random_array::<32>()), socket, now);
            }
            votes
        };
        // The runner-up must stay below 70% of the winner's votes.
        assert_eq!(votes(10, 7).majority(now), None);
        assert_eq!(votes(10, 6).majority(now), Some(a));
        assert_eq!(votes(9, 0).majority(now), None);
        // Votes expire after two minutes.
        assert_eq!(votes(10, 0).majority(now + VOTE_DURATION), None);
    }

    #[test]
    fn two_nodes_handshake_and_find_each_other() {
        let fork_id = ForkId { hash: [1, 2, 3, 4], next: 0 };
        let config = |key: SecretKey, bootnodes| Discv5Config {
            secret_key: key,
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            tcp_port: 30303,
            external_ip: None,
            fork_id,
            bootnodes,
        };
        let a_key = random_secret_key();
        let a = Discv5::start(config(a_key, vec![])).unwrap();
        let a_record = NodeRecord {
            id: peer_id(&PublicKey::from_secret_key_global(&a_key)),
            addr: SocketAddr::new(a.local_addr().ip(), 30303),
            udp_port: a.local_addr().port(),
        };
        let b = Discv5::start(config(random_secret_key(), vec![a_record])).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = Vec::new();
        while Instant::now() < deadline && found.is_empty() {
            thread::sleep(Duration::from_millis(50));
            found = b.take_found();
        }
        assert_eq!(found[0].record.id, a_record.id);
        assert_eq!(found[0].fork_id, Some(fork_id));
        assert_eq!(found[0].record.addr.port(), 30303);
        a.shutdown();
        b.shutdown();
    }
}
