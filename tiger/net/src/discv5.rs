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
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
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

/// Discovery configuration.
#[derive(Debug, Clone)]
pub struct Discv5Config {
    /// The node key.
    pub secret_key: SecretKey,
    /// UDP listen address.
    pub listen_addr: SocketAddr,
    /// The RLPx port to advertise (0 for none).
    pub tcp_port: u16,
    /// The IP to advertise; learned from PONGs when `None`.
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
    lookup_target: B256,
    lookup_asked: HashSet<B256>,
    lookup_rounds: u32,
    found: Vec<FoundNode>,
    next_request_id: u64,
    ip_learned: bool,
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
                ip_learned: ip.is_some(),
                local_enr,
                contacts,
                sessions: HashMap::new(),
                outgoing: HashMap::new(),
                challenges: HashMap::new(),
                buckets: vec![Vec::new(); 257],
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
        state.outgoing.insert(nonce, Outgoing { sent: Instant::now(), ..request });
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
        let Some(challenge) = state.challenges.remove(&src) else { return };
        if challenge.addr != from {
            return;
        }
        let public_key = if record.is_empty() {
            match state.contacts.get(&src) {
                Some(contact) => contact.public_key,
                None => return,
            }
        } else {
            let Ok(enr) = Enr::decode(&mut &record[..]) else { return };
            if enr.node_id() != src {
                return;
            }
            let key = *enr.public_key();
            self.add_contact(state, enr, from);
            key
        };
        let digest = id_signature_digest(&challenge.data, ephemeral, &self.local_id);
        if !verify_compact(digest, signature, &public_key) {
            return;
        }
        let Ok(ephemeral_key) = PublicKey::from_slice(ephemeral) else { return };
        let secret = ecdh_compressed(&ephemeral_key, &self.key);
        let (initiator_key, recipient_key) =
            derive_keys(&secret, &challenge.data, &src, &self.local_id);
        let Some(plaintext) = aes128_gcm_decrypt(
            &initiator_key,
            &packet.nonce,
            packet.message,
            &packet.iv_and_header(),
        ) else {
            return;
        };
        if state.sessions.len() >= MAX_SESSIONS {
            state.sessions.clear();
        }
        state.sessions.insert(src, Session { write_key: recipient_key, read_key: initiator_key });
        state.contacts.entry(src).or_insert(Contact { public_key, addr: from, enr: None });
        self.on_message(state, src, from, &plaintext);
    }

    fn on_message(&self, state: &mut State, src: B256, from: SocketAddr, plaintext: &[u8]) {
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
                // The response answers every outgoing request to `src`.
                state.outgoing.retain(|_, o| o.node != src);
                let _seq = u64::decode(&mut items);
                if !state.ip_learned
                    && let Ok(ip) = Bytes::decode(&mut items)
                    && let Ok(octets) = <[u8; 4]>::try_from(ip.as_ref())
                {
                    self.learn_ip(state, Ipv4Addr::from(octets));
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
                    self.add_contact(state, enr, udp);
                }
            }
            TALKREQ => {
                let message = encode_message(TALKRESP, &[&request_id, &&b""[..]]);
                self.send_response(state, src, from, &message);
            }
            _ => {}
        }
    }

    fn learn_ip(&self, state: &mut State, ip: Ipv4Addr) {
        state.ip_learned = true;
        let udp = self.socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let seq = state.local_enr.seq + 1;
        state.local_enr = Enr::build(
            &self.key,
            seq,
            Some(ip),
            self.config.tcp_port,
            udp,
            Some(self.config.fork_id),
        );
    }

    fn add_contact(&self, state: &mut State, enr: Enr, udp: SocketAddr) {
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
        let bucket = &mut state.buckets[distance];
        if !bucket.contains(&id) {
            if bucket.len() >= BUCKET_SIZE {
                // Full bucket: replace the oldest entry (bounded table, no liveness probing).
                let evicted = bucket.remove(0);
                state.contacts.remove(&evicted);
                state.sessions.remove(&evicted);
            }
            state.buckets[distance].push(id);
        }
        let public_key = *enr.public_key();
        state.contacts.insert(id, Contact { public_key, addr: udp, enr: Some(enr) });
    }

    /// Periodic work: expire requests and challenges, and advance the lookup.
    fn tick(&self) {
        let mut state = self.state.lock().unwrap();
        state.outgoing.retain(|_, o| o.sent.elapsed() < REQUEST_TIMEOUT);
        state.challenges.retain(|_, c| c.sent.elapsed() < REQUEST_TIMEOUT);
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
