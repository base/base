//! Live discv5 scenarios: a scripted discv5 v5.1 peer over UDP, written from the wire spec
//! (discv5-wire.md, discv5-theory.md) independently of the implementations under test. It masks
//! headers, runs the WHOAREYOU handshake and encrypts messages itself, so a scenario chooses every
//! header field, the id signature, the ENR it presents and the message bytes.
//!
//! Each scenario uses a fresh node key and its own 127.2.x.y source address, so no session, table
//! entry or rate limit carries over between scenarios. Outcomes name what came back (WHOAREYOU,
//! decrypted PONG / NODES / TALKRESP, requests the target sent us) or `silent`.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use aes::cipher::{KeyIvInit, StreamCipher};
use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use alloy_primitives::keccak256;
use hkdf::Hkdf;
use secp256k1::{Message, PublicKey, SECP256K1, Scalar, SecretKey};
use sha2::{Digest, Sha256};

use super::peer::Target;
use crate::el::{
    rlp::{self, Item, Node, b, int, l},
    rng::Rng,
    wire::{public_of_id, secret},
};

const BASEV0: [u8; 6] = *b"basev0";
const DISCV5: [u8; 6] = *b"discv5";
const FLAG_MESSAGE: u8 = 0;
const FLAG_WHOAREYOU: u8 = 1;
const FLAG_HANDSHAKE: u8 = 2;
const STATIC_HEADER: usize = 23;
const WAIT: Duration = Duration::from_millis(700);

/// The node id (keccak256 of the uncompressed public key without its prefix).
fn node_id(public: &PublicKey) -> [u8; 32] {
    keccak256(&public.serialize_uncompressed()[1..]).0
}

/// Logarithmic distance between two node ids (0 for equal ids).
fn log2_distance(a: &[u8; 32], b: &[u8; 32]) -> u64 {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = x ^ y;
        if d != 0 {
            return 256 - (i as u64 * 8 + d.leading_zeros() as u64);
        }
    }
    0
}

/// How the harness's ENR is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// A valid, canonical v4 record.
    Valid,
    /// No record in the handshake.
    Omitted,
    /// A valid record whose signature has one flipped byte.
    BadSignature,
    /// A record signed by (and naming) a different key than the handshake's source id.
    OtherKey,
    /// A valid record padded with an extra key to `n` bytes.
    Padded(usize),
    /// A valid record with keys out of order.
    Unsorted,
}

/// A deliberate fault in the handshake packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tamper {
    /// None.
    None,
    /// 33 random bytes as the ephemeral public key.
    EphemeralKey,
    /// One flipped ciphertext bit in the handshake message.
    Ciphertext,
}

/// The harness's node.
struct Local {
    key: SecretKey,
    id: [u8; 32],
    socket: UdpSocket,
    seq: u64,
}

impl Local {
    fn new(rng: &mut Rng, source: Ipv4Addr) -> Result<Self, String> {
        let key = secret(rng);
        let socket = UdpSocket::bind((source, 0)).map_err(|e| format!("bind {source}: {e}"))?;
        socket.set_read_timeout(Some(Duration::from_millis(20))).map_err(|e| e.to_string())?;
        Ok(Self { id: node_id(&key.public_key(SECP256K1)), key, socket, seq: 1 })
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// The RLP of a v4 record for `key` with the harness's address.
    fn record(&self, rng: &mut Rng, kind: Record) -> Option<Vec<u8>> {
        let key = match kind {
            Record::Omitted => return None,
            Record::OtherKey => secret(rng),
            _ => self.key,
        };
        let SocketAddr::V4(addr) = self.socket.local_addr().ok()? else { return None };
        let mut pairs: Vec<(&[u8], Node)> = vec![
            (b"id", b(b"v4".to_vec())),
            (b"ip", b(addr.ip().octets().to_vec())),
            (b"secp256k1", b(key.public_key(SECP256K1).serialize().to_vec())),
            (b"udp", int(addr.port() as u64)),
        ];
        if kind == Record::Unsorted {
            pairs.swap(0, 3);
        }
        let sign = |pairs: &[(&[u8], Node)], corrupt: bool| {
            let mut content = vec![int(self.seq)];
            for (k, v) in pairs {
                content.push(b(k.to_vec()));
                content.push(v.clone());
            }
            let digest = keccak256(l(content.clone()).encode());
            let mut sig =
                SECP256K1.sign_ecdsa(&Message::from_digest(digest.0), &key).serialize_compact();
            if corrupt {
                sig[10] ^= 1;
            }
            content.insert(0, b(sig.to_vec()));
            l(content).encode()
        };
        let mut out = sign(&pairs, kind == Record::BadSignature);
        if let Record::Padded(target) = kind {
            // "zz" sorts last; grow its value until the whole record hits the target size.
            let mut pad = target.saturating_sub(out.len() + 8);
            loop {
                let mut padded = pairs.clone();
                padded.push((b"zz", b(vec![0x61; pad])));
                out = sign(&padded, false);
                if out.len() >= target || pad > target {
                    break;
                }
                pad += 1;
            }
        }
        Some(out)
    }
}

/// A packet received from the target, unmasked.
struct Incoming {
    flag: u8,
    nonce: [u8; 12],
    authdata: Vec<u8>,
    /// masking-iv || static header || authdata: the AEAD associated data / challenge data.
    header: Vec<u8>,
    message: Vec<u8>,
}

/// An established session (keys from the initiator's side).
struct Session {
    send: [u8; 16],
    recv: [u8; 16],
}

/// The target as seen from the harness.
struct Remote {
    addr: SocketAddr,
    id: [u8; 32],
    public: PublicKey,
}

/// A packet's static header fields; `declared` is the authdata size it claims.
struct Static {
    protocol: [u8; 6],
    version: u16,
    flag: u8,
    nonce: [u8; 12],
    declared: u16,
}

/// Masks and assembles a packet: `iv || mask(static header || authdata) || message`.
fn packet(
    dest: &[u8; 32],
    iv: [u8; 16],
    fields: &Static,
    authdata: &[u8],
    message: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let mut header = fields.protocol.to_vec();
    header.extend_from_slice(&fields.version.to_be_bytes());
    header.push(fields.flag);
    header.extend_from_slice(&fields.nonce);
    header.extend_from_slice(&fields.declared.to_be_bytes());
    header.extend_from_slice(authdata);
    let mut ad = iv.to_vec();
    ad.extend_from_slice(&header);
    ctr::Ctr128BE::<aes::Aes128>::new(dest[..16].into(), (&iv).into()).apply_keystream(&mut header);
    let mut out = iv.to_vec();
    out.extend(header);
    out.extend_from_slice(message);
    (out, ad)
}

/// Unmasks a packet addressed to `local` (any protocol id).
fn unmask(local: &[u8; 32], data: &[u8]) -> Option<Incoming> {
    let iv: [u8; 16] = data.get(..16)?.try_into().ok()?;
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(local[..16].into(), (&iv).into());
    let mut header = data.get(16..16 + STATIC_HEADER)?.to_vec();
    cipher.apply_keystream(&mut header);
    let size = u16::from_be_bytes([header[21], header[22]]) as usize;
    let mut authdata = data.get(16 + STATIC_HEADER..16 + STATIC_HEADER + size)?.to_vec();
    cipher.apply_keystream(&mut authdata);
    let mut full = iv.to_vec();
    full.extend_from_slice(&header);
    full.extend_from_slice(&authdata);
    Some(Incoming {
        flag: header[8],
        nonce: header[9..21].try_into().ok()?,
        authdata,
        header: full,
        message: data[16 + STATIC_HEADER + size..].to_vec(),
    })
}

fn seal(key: &[u8; 16], nonce: &[u8; 12], plain: &[u8], ad: &[u8]) -> Vec<u8> {
    Aes128Gcm::new(key.into())
        .encrypt(Nonce::from_slice(nonce), Payload { msg: plain, aad: ad })
        .unwrap_or_default()
}

fn open(key: &[u8; 16], nonce: &[u8; 12], ct: &[u8], ad: &[u8]) -> Option<Vec<u8>> {
    Aes128Gcm::new(key.into()).decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: ad }).ok()
}

/// One scenario run: the harness node, the remote and what was observed.
struct Run<'a> {
    rng: &'a mut Rng,
    local: Local,
    remote: Remote,
    protocol: [u8; 6],
    session: Option<Session>,
    seen: Vec<String>,
}

impl Run<'_> {
    fn send_raw(&self, bytes: &[u8]) {
        let _ = self.local.socket.send_to(bytes, self.remote.addr);
    }

    /// Sends an ordinary message packet; with a session the message is encrypted.
    fn send_message(&mut self, plain: &[u8]) -> [u8; 12] {
        let nonce: [u8; 12] = self.rng.array();
        let iv: [u8; 16] = self.rng.array();
        let id = self.local.id;
        let (_, ad) = packet(
            &self.remote.id,
            iv,
            &Static {
                protocol: self.protocol,
                version: 1,
                flag: FLAG_MESSAGE,
                nonce,
                declared: 32,
            },
            &id,
            &[],
        );
        let message = self
            .session
            .as_ref()
            .map_or_else(|| plain.to_vec(), |s| seal(&s.send, &nonce, plain, &ad));
        let (bytes, _) = packet(
            &self.remote.id,
            iv,
            &Static {
                protocol: self.protocol,
                version: 1,
                flag: FLAG_MESSAGE,
                nonce,
                declared: 32,
            },
            &id,
            &message,
        );
        self.send_raw(&bytes);
        nonce
    }

    /// Collects packets for `window`, answering nothing; describes each.
    fn observe(&self, window: Duration) -> Vec<(String, Option<Incoming>)> {
        let mut out = Vec::new();
        let deadline = Instant::now() + window;
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            let Ok((len, from)) = self.local.socket.recv_from(&mut buf) else { continue };
            if from != self.remote.addr {
                continue;
            }
            let Some(packet) = unmask(&self.local.id, &buf[..len]) else {
                out.push((format!("garbled({len})"), None));
                continue;
            };
            let text = self.describe(&packet);
            out.push((text, Some(packet)));
        }
        out
    }

    fn describe(&self, p: &Incoming) -> String {
        match p.flag {
            FLAG_WHOAREYOU if p.authdata.len() == 24 => {
                format!(
                    "WHOAREYOU(enr-seq={})",
                    u64::from_be_bytes(p.authdata[16..24].try_into().unwrap())
                )
            }
            FLAG_MESSAGE => self.session.as_ref().map_or_else(
                || "message-without-session".into(),
                |s| {
                    open(&s.recv, &p.nonce, &p.message, &p.header).map_or_else(
                        || "undecryptable-message".into(),
                        |plain| describe_message(&plain, &self.local),
                    )
                },
            ),
            FLAG_HANDSHAKE => "handshake".into(),
            other => format!("flag-{other}"),
        }
    }

    /// Sends an ordinary message from an unknown node and waits for the WHOAREYOU it triggers.
    fn challenge(&mut self) -> Option<Incoming> {
        let junk = self.rng.bytes(24);
        let nonce = self.send_message(&junk);
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            for (text, packet) in self.observe(Duration::from_millis(50)) {
                match packet {
                    Some(p) if p.flag == FLAG_WHOAREYOU && p.nonce == nonce => return Some(p),
                    _ => self.seen.push(text),
                }
            }
        }
        None
    }

    /// Runs the handshake, sending `first` (a plaintext message) in the handshake packet.
    fn handshake(
        &mut self,
        record: Record,
        bad_id_signature: bool,
        first: &[u8],
        tamper: Tamper,
    ) -> Result<String, String> {
        let challenge = self.challenge().ok_or("no WHOAREYOU")?;
        let remote_seq = u64::from_be_bytes(challenge.authdata[16..24].try_into().unwrap());
        let eph = secret(self.rng);
        let mut eph_pub = eph.public_key(SECP256K1).serialize().to_vec();
        let shared = self
            .remote
            .public
            .mul_tweak(SECP256K1, &Scalar::from(eph))
            .map_err(|e| e.to_string())?
            .serialize();
        let mut info = b"discovery v5 key agreement".to_vec();
        info.extend_from_slice(&self.local.id);
        info.extend_from_slice(&self.remote.id);
        let mut keys = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&challenge.header), &shared)
            .expand(&info, &mut keys)
            .map_err(|e| e.to_string())?;
        let mut proof = Sha256::new();
        proof.update(b"discovery v5 identity proof");
        proof.update(&challenge.header);
        proof.update(&eph_pub);
        proof.update(self.remote.id);
        let mut sig = SECP256K1
            .sign_ecdsa(&Message::from_digest(proof.finalize().into()), &self.local.key)
            .serialize_compact();
        if bad_id_signature {
            sig[5] ^= 1;
        }
        if tamper == Tamper::EphemeralKey {
            eph_pub = self.rng.bytes(33);
        }
        let mut authdata = self.local.id.to_vec();
        authdata.push(64);
        authdata.push(33);
        authdata.extend_from_slice(&sig);
        authdata.extend_from_slice(&eph_pub);
        if let Some(rec) = self.local.record(self.rng, record) {
            authdata.extend(rec);
        }
        let nonce: [u8; 12] = self.rng.array();
        let iv: [u8; 16] = self.rng.array();
        let declared = authdata.len() as u16;
        let (_, ad) = packet(
            &self.remote.id,
            iv,
            &Static { protocol: self.protocol, version: 1, flag: FLAG_HANDSHAKE, nonce, declared },
            &authdata,
            &[],
        );
        let send: [u8; 16] = keys[..16].try_into().unwrap();
        let mut message = seal(&send, &nonce, first, &ad);
        if tamper == Tamper::Ciphertext {
            message[0] ^= 1;
        }
        let (bytes, _) = packet(
            &self.remote.id,
            iv,
            &Static { protocol: self.protocol, version: 1, flag: FLAG_HANDSHAKE, nonce, declared },
            &authdata,
            &message,
        );
        self.send_raw(&bytes);
        self.session = Some(Session { send, recv: keys[16..].try_into().unwrap() });
        Ok(format!("challenge enr-seq={remote_seq}"))
    }

    /// Whether another fresh node finds the harness node in the target's table.
    fn in_table(&mut self, source: Ipv4Addr) -> String {
        let distance = log2_distance(&self.remote.id, &self.local.id);
        let Ok(local) = Local::new(self.rng, source) else { return "probe-bind-failed".into() };
        let remote =
            Remote { addr: self.remote.addr, id: self.remote.id, public: self.remote.public };
        let mut probe = Run {
            rng: self.rng,
            local,
            remote,
            protocol: self.protocol,
            session: None,
            seen: Vec::new(),
        };
        let findnode = message(3, vec![b(vec![9]), l(vec![int(distance)])]);
        if probe.handshake(Record::Valid, false, &findnode, Tamper::None).is_err() {
            return "probe-no-handshake".into();
        }
        let mut found = false;
        for (_, packet) in probe.observe(WAIT) {
            let Some(p) = packet else { continue };
            let Some(s) = &probe.session else { continue };
            let Some(plain) = open(&s.recv, &p.nonce, &p.message, &p.header) else { continue };
            if plain.first() == Some(&4) {
                // A record carries the compressed public key; look for ours.
                let ours = self.local.key.public_key(SECP256K1).serialize();
                found |= plain.windows(33).any(|w| w == ours);
            }
        }
        format!("in-table={found}")
    }
}

/// `type || rlp(list)`.
fn message(kind: u8, items: Vec<Node>) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend(l(items).encode());
    out
}

fn as_u64(node: &Node) -> Option<u64> {
    match &node.item {
        Item::Bytes(v) if v.len() <= 8 => {
            Some(v.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64))
        }
        _ => None,
    }
}

fn describe_message(plain: &[u8], local: &Local) -> String {
    let Some((&kind, body)) = plain.split_first() else { return "empty-message".into() };
    let Some((Node { item: Item::List(items), .. }, _)) = rlp::parse(body) else {
        return format!("type-{kind}(undecodable)");
    };
    let n = |i: usize| items.get(i).and_then(as_u64);
    match kind {
        1 => format!("PING(enr-seq={:?})", n(1)),
        2 => {
            let ip = match items.get(2).map(|x| &x.item) {
                Some(Item::Bytes(v)) => {
                    v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".")
                }
                _ => "?".into(),
            };
            let port_ok = n(3) == Some(local.port() as u64);
            format!("PONG(enr-seq={:?}, ip={ip}, port-matches={port_ok})", n(1))
        }
        3 => format!(
            "FINDNODE({:?})",
            items.get(1).map(|d| match &d.item {
                Item::List(ds) => ds.iter().map(as_u64).collect::<Vec<_>>(),
                Item::Bytes(_) => vec![],
            })
        ),
        4 => {
            let records = match items.get(2).map(|x| &x.item) {
                Some(Item::List(rs)) => rs.len(),
                _ => 0,
            };
            format!("NODES(total={:?}, records={records})", n(1))
        }
        5 => "TALKREQ".into(),
        6 => {
            let len = match items.get(1).map(|x| &x.item) {
                Some(Item::Bytes(v)) => v.len(),
                _ => usize::MAX,
            };
            format!("TALKRESP(len={len})")
        }
        other => format!("type-{other}"),
    }
}

/// What a scenario does.
type Body = fn(&mut Run<'_>, Ipv4Addr) -> String;

/// Ordinary packet with a header tweak, no session.
fn raw(
    run: &mut Run<'_>,
    protocol: [u8; 6],
    version: u16,
    flag: u8,
    declared: u16,
    authdata_len: usize,
    message_len: usize,
) -> String {
    let nonce: [u8; 12] = run.rng.array();
    let iv: [u8; 16] = run.rng.array();
    let mut authdata = run.local.id.to_vec();
    authdata.resize(authdata_len, 0);
    let message = run.rng.bytes(message_len);
    let (bytes, _) = packet(
        &run.remote.id,
        iv,
        &Static { protocol, version, flag, nonce, declared },
        &authdata,
        &message,
    );
    run.send_raw(&bytes);
    summarize(run.observe(WAIT))
}

/// Responses in arrival order (consecutive NODES merged into one entry with the record count),
/// then the set of requests the target started on its own, which are lookup traffic except for
/// `FINDNODE([0])`, the request for an updated record.
fn summarize(events: Vec<(String, Option<Incoming>)>) -> String {
    let mut responses: Vec<String> = Vec::new();
    let (mut nodes_msgs, mut nodes_records) = (0, 0);
    let mut requests = BTreeSet::new();
    for (text, _) in events {
        if let Some(rest) = text.strip_prefix("NODES(") {
            nodes_msgs += 1;
            nodes_records += rest
                .split("records=")
                .nth(1)
                .and_then(|r| r.trim_end_matches(')').parse::<usize>().ok())
                .unwrap_or(0);
        } else if text.starts_with("FINDNODE(Some([Some(0)]))") {
            requests.insert("FINDNODE([0])".to_string());
        } else if text.starts_with("FINDNODE")
            || text.starts_with("PING")
            || text == "message-without-session"
        {
            requests.insert(text.split('(').next().unwrap_or("").to_string());
        } else if text == "undecryptable-message" {
            // The target talking to us under keys we do not hold: a request sent without a session.
            requests.insert("sessionless-request".to_string());
        } else {
            responses.push(text);
        }
    }
    if nodes_msgs > 0 {
        responses.push(format!("NODES(packets={nodes_msgs}, records={nodes_records})"));
    }
    let mut out = if responses.is_empty() { "silent".to_string() } else { responses.join(" + ") };
    if !requests.is_empty() {
        out.push_str(&format!(
            " [target requests: {}]",
            requests.into_iter().collect::<Vec<_>>().join(",")
        ));
    }
    out
}

/// Handshake carrying `first`, then everything the target sends back.
fn after_handshake(
    run: &mut Run<'_>,
    record: Record,
    bad_sig: bool,
    first: Vec<u8>,
    tamper: Tamper,
) -> String {
    match run.handshake(record, bad_sig, &first, tamper) {
        Ok(_) => summarize(run.observe(WAIT)),
        Err(e) => e,
    }
}

fn ping(seq: u64) -> Vec<u8> {
    message(1, vec![b(vec![1]), int(seq)])
}

fn findnode(distances: Vec<Node>) -> Vec<u8> {
    message(3, vec![b(vec![2]), l(distances)])
}

fn ints(values: &[u64]) -> Vec<Node> {
    values.iter().map(|v| int(*v)).collect()
}

/// Handshake with a valid record and PING, then `next` in the session.
fn in_session(run: &mut Run<'_>, next: Vec<u8>) -> String {
    if let Err(e) = run.handshake(Record::Valid, false, &ping(1), Tamper::None) {
        return e;
    }
    let first = summarize(run.observe(Duration::from_millis(300)));
    run.send_message(&next);
    format!("{first} | {}", summarize(run.observe(WAIT)))
}

/// Handshake with `record` + PING, then whether the node landed in the target's table.
fn record_case(run: &mut Run<'_>, source: Ipv4Addr, record: Record) -> String {
    let answer = after_handshake(run, record, false, ping(1), Tamper::None);
    format!("{answer} | {}", run.in_table(source))
}

fn scenarios() -> Vec<(&'static str, &'static str, Body)> {
    vec![
        ("packet", "message/basev0", |r, _| raw(r, BASEV0, 1, 0, 32, 32, 24)),
        ("packet", "message/discv5-protocol-id", |r, _| raw(r, DISCV5, 1, 0, 32, 32, 24)),
        ("packet", "message/version-0", |r, _| raw(r, r.protocol, 0, 0, 32, 32, 24)),
        ("packet", "message/version-2", |r, _| raw(r, r.protocol, 2, 0, 32, 32, 24)),
        ("packet", "message/flag-3", |r, _| raw(r, r.protocol, 1, 3, 32, 32, 24)),
        ("packet", "message/authdata-31", |r, _| raw(r, r.protocol, 1, 0, 31, 31, 24)),
        ("packet", "message/authdata-33", |r, _| raw(r, r.protocol, 1, 0, 33, 33, 24)),
        ("packet", "message/authdata-declared-past-end", |r, _| {
            raw(r, r.protocol, 1, 0, 2000, 32, 24)
        }),
        ("packet", "message/empty-message-71-bytes", |r, _| raw(r, r.protocol, 1, 0, 32, 32, 0)),
        ("packet", "message/62-bytes", |r, _| raw(r, r.protocol, 1, 0, 32, 23, 0)),
        ("packet", "message/1280-bytes", |r, _| raw(r, r.protocol, 1, 0, 32, 32, 1280 - 71)),
        ("packet", "message/1281-bytes", |r, _| raw(r, r.protocol, 1, 0, 32, 32, 1281 - 71)),
        ("packet", "whoareyou-unsolicited", |r, _| {
            raw(r, r.protocol, 1, FLAG_WHOAREYOU, 24, 24, 0)
        }),
        ("packet", "handshake-unsolicited", |r, _| {
            raw(r, r.protocol, 1, FLAG_HANDSHAKE, 34 + 64 + 33, 34 + 64 + 33, 40)
        }),
        ("handshake", "valid-record-ping", |r, _| {
            after_handshake(r, Record::Valid, false, ping(1), Tamper::None)
        }),
        ("handshake", "no-record-when-requested", |r, _| {
            after_handshake(r, Record::Omitted, false, ping(1), Tamper::None)
        }),
        ("handshake", "bad-id-signature", |r, _| {
            after_handshake(r, Record::Valid, true, ping(1), Tamper::None)
        }),
        ("handshake", "garbage-ephemeral-key", |r, _| {
            after_handshake(r, Record::Valid, false, ping(1), Tamper::EphemeralKey)
        }),
        ("handshake", "undecryptable-message", |r, _| {
            after_handshake(r, Record::Valid, false, ping(1), Tamper::Ciphertext)
        }),
        ("record", "valid", |r, s| record_case(r, s, Record::Valid)),
        ("record", "valid-record-bad-id-signature", |r, s| {
            let answer = after_handshake(r, Record::Valid, true, ping(1), Tamper::None);
            format!("{answer} | {}", r.in_table(s))
        }),
        ("record", "valid-record-garbage-ephemeral-key", |r, s| {
            let answer = after_handshake(r, Record::Valid, false, ping(1), Tamper::EphemeralKey);
            format!("{answer} | {}", r.in_table(s))
        }),
        ("record", "valid-record-undecryptable-message", |r, s| {
            let answer = after_handshake(r, Record::Valid, false, ping(1), Tamper::Ciphertext);
            format!("{answer} | {}", r.in_table(s))
        }),
        ("record", "250-bytes", |r, s| record_case(r, s, Record::Padded(250))),
        ("record", "bad-signature", |r, s| record_case(r, s, Record::BadSignature)),
        ("record", "other-key", |r, s| record_case(r, s, Record::OtherKey)),
        ("record", "unsorted-keys", |r, s| record_case(r, s, Record::Unsorted)),
        ("record", "300-bytes", |r, s| record_case(r, s, Record::Padded(300))),
        ("record", "301-bytes", |r, s| record_case(r, s, Record::Padded(301))),
        ("record", "600-bytes", |r, s| record_case(r, s, Record::Padded(600))),
        ("findnode", "distance-0", |r, _| in_session(r, findnode(ints(&[0])))),
        ("findnode", "distance-256", |r, _| in_session(r, findnode(ints(&[256])))),
        ("findnode", "distance-257", |r, _| in_session(r, findnode(ints(&[257])))),
        ("findnode", "distance-u64-max", |r, _| in_session(r, findnode(ints(&[u64::MAX])))),
        ("findnode", "no-distances", |r, _| in_session(r, findnode(vec![]))),
        ("findnode", "3-distances", |r, _| in_session(r, findnode(ints(&[256, 255, 254])))),
        ("findnode", "4-distances", |r, _| in_session(r, findnode(ints(&[256, 255, 254, 253])))),
        ("findnode", "duplicate-distances", |r, _| in_session(r, findnode(ints(&[0, 0, 0])))),
        ("findnode", "0-and-256", |r, _| in_session(r, findnode(ints(&[0, 256])))),
        ("findnode", "distance-leading-zero", |r, _| in_session(r, findnode(vec![b(vec![0, 1])]))),
        ("findnode", "distances-not-a-list", |r, _| {
            in_session(r, message(3, vec![b(vec![2]), int(256)]))
        }),
        ("findnode", "own-distance", |r, _| {
            let d = log2_distance(&r.remote.id, &r.local.id);
            in_session(r, findnode(ints(&[d])))
        }),
        ("message", "ping-higher-enr-seq", |r, _| in_session(r, ping(9))),
        ("message", "ping-request-id-9-bytes", |r, _| {
            in_session(r, message(1, vec![b(vec![1; 9]), int(1)]))
        }),
        ("message", "ping-request-id-empty", |r, _| {
            in_session(r, message(1, vec![b(vec![]), int(1)]))
        }),
        ("message", "ping-extra-field", |r, _| {
            in_session(r, message(1, vec![b(vec![1]), int(1), int(7)]))
        }),
        ("message", "ping-missing-seq", |r, _| in_session(r, message(1, vec![b(vec![1])]))),
        ("message", "ping-trailing-bytes", |r, _| {
            let mut m = ping(1);
            m.push(0);
            in_session(r, m)
        }),
        ("message", "unknown-type-9", |r, _| in_session(r, message(9, vec![b(vec![1])]))),
        ("message", "empty-plaintext", |r, _| in_session(r, vec![])),
        ("message", "talkreq-unknown-protocol", |r, _| {
            in_session(r, message(5, vec![b(vec![3]), b(b"nope".to_vec()), b(vec![1, 2, 3])]))
        }),
        ("message", "pong-unsolicited", |r, _| {
            in_session(r, message(2, vec![b(vec![4]), int(1), b(vec![127, 0, 0, 1]), int(1)]))
        }),
        ("message", "nodes-unsolicited", |r, _| {
            in_session(r, message(4, vec![b(vec![4]), int(1), l(vec![])]))
        }),
    ]
}

/// Runs `el live discv5`.
pub(crate) fn run(targets: &[String], seed: u64, out: Option<&str>) -> Result<(), String> {
    let targets: Vec<Target> =
        targets.iter().map(|t| Target::parse(t)).collect::<Result<_, _>>()?;
    let mut rows = Vec::new();
    // Protocol id per target: the one it answers WHOAREYOU on.
    let mut protocols = BTreeMap::new();
    for (i, target) in targets.iter().enumerate() {
        let mut rng = Rng::case(seed, 0xd15c);
        let mut chosen = BASEV0;
        for protocol in [BASEV0, DISCV5] {
            let mut run =
                start(&mut rng, target, protocol, Ipv4Addr::new(127, 2, 250, i as u8 * 2 + 2))?;
            if run.challenge().is_some() {
                chosen = protocol;
                break;
            }
        }
        println!("{}: protocol id {}", target.name, String::from_utf8_lossy(&chosen));
        protocols.insert(target.name.clone(), chosen);
    }
    for (index, (group, name, body)) in scenarios().into_iter().enumerate() {
        let source = Ipv4Addr::new(127, 2, (index / 120) as u8, (index % 120 * 2 + 2) as u8);
        let probe = Ipv4Addr::new(127, 2, (index / 120) as u8, (index % 120 * 2 + 3) as u8);
        let mut outcomes = BTreeMap::new();
        for target in &targets {
            let mut rng = Rng::case(seed, index as u64);
            let outcome = match start(&mut rng, target, protocols[&target.name], source) {
                Ok(mut run) => {
                    let mut text = body(&mut run, probe);
                    if !run.seen.is_empty() {
                        text.push_str(&format!(" (also: {})", run.seen.join(", ")));
                    }
                    text
                }
                Err(e) => e,
            };
            outcomes.insert(target.name.clone(), outcome);
        }
        let differ = outcomes.values().collect::<BTreeSet<_>>().len() > 1;
        println!("{} {group}/{name}", if differ { "DIFF" } else { "same" });
        for (n, o) in &outcomes {
            println!("    {n:<11} {o}");
        }
        rows.push(serde_json::json!({ "group": group, "scenario": name, "outcomes": outcomes }));
        std::thread::sleep(Duration::from_millis(100));
    }
    let diffs = rows.iter().filter(|r| {
        r["outcomes"]
            .as_object()
            .map(|o| o.values().map(|v| v.to_string()).collect::<BTreeSet<_>>().len() > 1)
            .unwrap_or(false)
    });
    println!("== {} scenarios, {} with differing outcomes ==", rows.len(), diffs.count());
    if let Some(path) = out {
        std::fs::write(path, serde_json::to_string_pretty(&rows).unwrap())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn start<'a>(
    rng: &'a mut Rng,
    target: &Target,
    protocol: [u8; 6],
    source: Ipv4Addr,
) -> Result<Run<'a>, String> {
    let public = public_of_id(&target.id).ok_or("bad node id")?;
    let remote = Remote { addr: target.addr, id: node_id(&public), public };
    let local = Local::new(rng, source)?;
    Ok(Run { rng, local, remote, protocol, session: None, seen: Vec::new() })
}
