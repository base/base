//! tp2p messages and their encoding.
//!
//! A message is a type byte and a body. Integers are big-endian, a byte string is a `u32` length
//! and the bytes, and a list is a `u32` count and its items. A trailing byte string takes the
//! rest of the message, without a length. Every decoder checks a length against the bytes
//! present before it allocates, and refuses trailing bytes, so each value has exactly one
//! encoding.

use secp256k1::{Message as Digest32, PublicKey, SECP256K1, SecretKey, ecdsa::Signature};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::noise::MAX_MESSAGE_LEN;

/// tp2p's protocol version, sent in [`Status`].
pub const PROTOCOL_VERSION: u16 = 1;
/// A gossip message id: the first 20 bytes of SHA-256(topic || data).
pub type MessageId = [u8; 20];
/// The most records one [`Message::Peers`] carries.
pub const MAX_PEER_RECORDS: usize = 64;
/// The most ids one [`Message::IHave`] or [`Message::IWant`] carries.
pub const MAX_IDS: usize = 256;

/// Why a peer closes a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DisconnectReason {
    /// Ordinary close (shutdown, peer no longer needed).
    Requested = 0,
    /// The peer broke the protocol (bad message, bad order, a refused limit).
    ProtocolViolation = 1,
    /// Too many peers.
    TooManyPeers = 2,
    /// Already connected to this peer.
    Duplicate = 3,
    /// The peer sent invalid data (gossip rejected too often, bad responses).
    Misbehaving = 4,
    /// The peer did not answer pings, or could not keep up.
    Timeout = 5,
    /// Another tp2p version or network.
    Incompatible = 6,
}

impl DisconnectReason {
    fn from_byte(b: u8) -> Result<Self, WireError> {
        Ok(match b {
            0 => Self::Requested,
            1 => Self::ProtocolViolation,
            2 => Self::TooManyPeers,
            3 => Self::Duplicate,
            4 => Self::Misbehaving,
            5 => Self::Timeout,
            6 => Self::Incompatible,
            _ => return Err(WireError::BadValue("disconnect reason")),
        })
    }
}

/// What a peer tells another right after the handshake, before anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// [`PROTOCOL_VERSION`].
    pub version: u16,
    /// Capability bits (application-defined: which request kinds and topics it serves).
    pub capabilities: u32,
    /// The sender's head block number.
    pub head_number: u64,
    /// The sender's head block hash.
    pub head_hash: [u8; 32],
    /// The port the sender listens on (0 when it does not), so the receiver can record the
    /// dialable address of an inbound peer.
    pub listen_port: u16,
}

/// A peer's signed, dialable address. The signature covers the network id, so a record from
/// one network does not verify on another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    /// The peer's static key (its identity).
    pub key: PublicKey,
    /// Where to dial it.
    pub addr: SocketAddr,
    /// Increases each time the peer changes the record; the highest wins.
    pub seq: u64,
    /// The peer's capability bits.
    pub capabilities: u32,
    /// Compact ECDSA signature by `key` over [`PeerRecord::digest`].
    pub signature: [u8; 64],
}

impl PeerRecord {
    /// Builds and signs a record.
    pub fn signed(
        secret: &SecretKey,
        network_id: &[u8; 32],
        addr: SocketAddr,
        seq: u64,
        capabilities: u32,
    ) -> Self {
        let key = PublicKey::from_secret_key(SECP256K1, secret);
        let mut record = Self { key, addr, seq, capabilities, signature: [0; 64] };
        let digest = Digest32::from_digest(record.digest(network_id));
        record.signature = SECP256K1.sign_ecdsa(digest, secret).serialize_compact();
        record
    }

    /// SHA-256 of a domain tag, the network id and the record's fields.
    pub fn digest(&self, network_id: &[u8; 32]) -> [u8; 32] {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"tp2p-record");
        buf.extend_from_slice(network_id);
        let mut fields = Encoder(buf);
        fields.record_fields(self);
        crate::noise::sha256(&[&fields.0])
    }

    /// Whether the signature is valid for this network. Non-canonical (high-s) signatures fail.
    pub fn verify(&self, network_id: &[u8; 32]) -> bool {
        let Ok(signature) = Signature::from_compact(&self.signature) else { return false };
        let digest = Digest32::from_digest(self.digest(network_id));
        SECP256K1.verify_ecdsa(digest, &signature, &self.key).is_ok()
    }
}

/// Answer codes of a [`Message::Response`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseCode {
    /// The body is the answer.
    Ok = 0,
    /// The responder does not have what was asked for (yet).
    NotFound = 1,
    /// The responder is over its rate or concurrency limit; ask later or elsewhere.
    Busy = 2,
    /// The request was malformed or of an unknown kind.
    BadRequest = 3,
}

impl ResponseCode {
    fn from_byte(b: u8) -> Result<Self, WireError> {
        Ok(match b {
            0 => Self::Ok,
            1 => Self::NotFound,
            2 => Self::Busy,
            3 => Self::BadRequest,
            _ => return Err(WireError::BadValue("response code")),
        })
    }
}

/// A tp2p message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// First message on a connection, in each direction.
    Status(Status),
    /// Liveness check; answered by `Pong` with the same nonce.
    Ping(u64),
    /// Answer to `Ping`.
    Pong(u64),
    /// The sender is closing the connection.
    Disconnect(DisconnectReason),
    /// Asks for up to this many peer records.
    GetPeers(u16),
    /// Signed records of peers the sender knows.
    Peers(Vec<PeerRecord>),
    /// An application request of `kind`, answered by a `Response` with the same id.
    Request {
        /// Chosen by the requester, unique among its outstanding requests to this peer.
        id: u32,
        /// Application-defined request kind.
        kind: u16,
        /// Application-defined body.
        body: Vec<u8>,
    },
    /// The answer to a `Request`.
    Response {
        /// The request's id.
        id: u32,
        /// Whether `body` is an answer.
        code: ResponseCode,
        /// The answer, empty unless `code` is `Ok`.
        body: Vec<u8>,
    },
    /// A gossip message: forwarded by each receiver only after its application accepts it.
    Gossip {
        /// Application-defined topic.
        topic: u8,
        /// The message.
        data: Vec<u8>,
    },
    /// Ids of gossip messages the sender has accepted, for receivers that lack them.
    IHave(Vec<MessageId>),
    /// Asks for the gossip messages with these ids.
    IWant(Vec<MessageId>),
    /// The sender already has these messages (received, maybe not yet validated): do not send
    /// them to it. Cuts duplicate transfers of large messages, as gossipsub 1.2's IDONTWANT.
    IDontWant(Vec<MessageId>),
    /// A one-hop application notice (not forwarded), such as a block announcement.
    Notify {
        /// Application-defined kind.
        kind: u16,
        /// Application-defined body.
        body: Vec<u8>,
    },
}

/// Message type bytes.
mod ty {
    pub(super) const STATUS: u8 = 0x00;
    pub(super) const PING: u8 = 0x01;
    pub(super) const PONG: u8 = 0x02;
    pub(super) const DISCONNECT: u8 = 0x03;
    pub(super) const GET_PEERS: u8 = 0x04;
    pub(super) const PEERS: u8 = 0x05;
    pub(super) const REQUEST: u8 = 0x10;
    pub(super) const RESPONSE: u8 = 0x11;
    pub(super) const GOSSIP: u8 = 0x20;
    pub(super) const IHAVE: u8 = 0x21;
    pub(super) const IWANT: u8 = 0x22;
    pub(super) const NOTIFY: u8 = 0x23;
    pub(super) const IDONTWANT: u8 = 0x24;
}

impl Message {
    /// The message's type byte, which the framing sends ahead of the body.
    pub const fn type_byte(&self) -> u8 {
        match self {
            Self::Status(_) => ty::STATUS,
            Self::Ping(_) => ty::PING,
            Self::Pong(_) => ty::PONG,
            Self::Disconnect(_) => ty::DISCONNECT,
            Self::GetPeers(_) => ty::GET_PEERS,
            Self::Peers(_) => ty::PEERS,
            Self::Request { .. } => ty::REQUEST,
            Self::Response { .. } => ty::RESPONSE,
            Self::Gossip { .. } => ty::GOSSIP,
            Self::IHave(_) => ty::IHAVE,
            Self::IWant(_) => ty::IWANT,
            Self::Notify { .. } => ty::NOTIFY,
            Self::IDontWant(_) => ty::IDONTWANT,
        }
    }

    /// Whether the message travels on the bulk stream (large application data) rather than the
    /// priority stream, so a large response never delays gossip or control messages.
    pub const fn is_bulk(&self) -> bool {
        matches!(self, Self::Request { .. } | Self::Response { .. })
    }

    /// Encodes the body (without the type byte).
    pub fn encode_body(&self) -> Vec<u8> {
        let mut e = Encoder(Vec::new());
        match self {
            Self::Status(s) => {
                e.u16(s.version);
                e.u32(s.capabilities);
                e.u64(s.head_number);
                e.bytes_fixed(&s.head_hash);
                e.u16(s.listen_port);
            }
            Self::Ping(n) | Self::Pong(n) => e.u64(*n),
            Self::Disconnect(r) => e.u8(*r as u8),
            Self::GetPeers(n) => e.u16(*n),
            Self::Peers(records) => {
                e.u32(records.len() as u32);
                for r in records {
                    e.record_fields(r);
                    e.bytes_fixed(&r.signature);
                }
            }
            Self::Request { id, kind, body } => {
                e.u32(*id);
                e.u16(*kind);
                e.bytes_fixed(body);
            }
            Self::Response { id, code, body } => {
                e.u32(*id);
                e.u8(*code as u8);
                e.bytes_fixed(body);
            }
            Self::Gossip { topic, data } => {
                e.u8(*topic);
                e.bytes_fixed(data);
            }
            Self::IHave(ids) | Self::IWant(ids) | Self::IDontWant(ids) => {
                e.u32(ids.len() as u32);
                for id in ids {
                    e.bytes_fixed(id);
                }
            }
            Self::Notify { kind, body } => {
                e.u16(*kind);
                e.bytes_fixed(body);
            }
        }
        e.0
    }

    /// Decodes a message from its type byte and body.
    pub fn decode(type_byte: u8, body: &[u8]) -> Result<Self, WireError> {
        let mut d = Decoder { buf: body };
        let message = match type_byte {
            ty::STATUS => Self::Status(Status {
                version: d.u16()?,
                capabilities: d.u32()?,
                head_number: d.u64()?,
                head_hash: d.array()?,
                listen_port: d.u16()?,
            }),
            ty::PING => Self::Ping(d.u64()?),
            ty::PONG => Self::Pong(d.u64()?),
            ty::DISCONNECT => Self::Disconnect(DisconnectReason::from_byte(d.u8()?)?),
            ty::GET_PEERS => Self::GetPeers(d.u16()?),
            ty::PEERS => {
                let count = d.count(MAX_PEER_RECORDS, 33 + 1 + 4 + 2 + 8 + 4 + 64)?;
                let mut records = Vec::with_capacity(count);
                for _ in 0..count {
                    records.push(d.record()?);
                }
                Self::Peers(records)
            }
            ty::REQUEST => Self::Request { id: d.u32()?, kind: d.u16()?, body: d.rest() },
            ty::RESPONSE => {
                let id = d.u32()?;
                let code = ResponseCode::from_byte(d.u8()?)?;
                let body = d.rest();
                if code != ResponseCode::Ok && !body.is_empty() {
                    return Err(WireError::BadValue("body on an error response"));
                }
                Self::Response { id, code, body }
            }
            ty::GOSSIP => Self::Gossip { topic: d.u8()?, data: d.rest() },
            ty::IHAVE | ty::IWANT | ty::IDONTWANT => {
                let count = d.count(MAX_IDS, 20)?;
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    ids.push(d.array()?);
                }
                match type_byte {
                    ty::IHAVE => Self::IHave(ids),
                    ty::IWANT => Self::IWant(ids),
                    _ => Self::IDontWant(ids),
                }
            }
            ty::NOTIFY => Self::Notify { kind: d.u16()?, body: d.rest() },
            other => return Err(WireError::UnknownType(other)),
        };
        if !d.buf.is_empty() {
            return Err(WireError::TrailingBytes);
        }
        Ok(message)
    }
}

/// Why a message did not decode. Any of these ends the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The body ended early.
    Truncated,
    /// Bytes left after the last field.
    TrailingBytes,
    /// An unassigned message type byte.
    UnknownType(u8),
    /// A field out of its range.
    BadValue(&'static str),
    /// A list longer than its limit.
    TooMany,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated"),
            Self::TrailingBytes => f.write_str("trailing bytes"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
            Self::BadValue(what) => write!(f, "bad {what}"),
            Self::TooMany => f.write_str("list too long"),
        }
    }
}

impl std::error::Error for WireError {}

struct Encoder(Vec<u8>);

impl Encoder {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes_fixed(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn record_fields(&mut self, r: &PeerRecord) {
        self.bytes_fixed(&r.key.serialize());
        match r.addr.ip() {
            IpAddr::V4(ip) => {
                self.u8(4);
                self.bytes_fixed(&ip.octets());
            }
            IpAddr::V6(ip) => {
                self.u8(6);
                self.bytes_fixed(&ip.octets());
            }
        }
        self.u16(r.addr.port());
        self.u64(r.seq);
        self.u32(r.capabilities);
    }
}

struct Decoder<'a> {
    buf: &'a [u8],
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], WireError> {
        if self.buf.len() < n {
            return Err(WireError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    /// A list count, checked against its limit and against the bytes left for items of at least
    /// `min_item` bytes, before anything is allocated.
    fn count(&mut self, max: usize, min_item: usize) -> Result<usize, WireError> {
        let count = self.u32()? as usize;
        if count > max {
            return Err(WireError::TooMany);
        }
        if count.saturating_mul(min_item) > self.buf.len() {
            return Err(WireError::Truncated);
        }
        Ok(count)
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf).to_vec()
    }
    fn record(&mut self) -> Result<PeerRecord, WireError> {
        let key =
            PublicKey::from_slice(self.take(33)?).map_err(|_| WireError::BadValue("record key"))?;
        let ip = match self.u8()? {
            4 => IpAddr::V4(Ipv4Addr::from(self.array::<4>()?)),
            6 => IpAddr::V6(Ipv6Addr::from(self.array::<16>()?)),
            _ => return Err(WireError::BadValue("record address family")),
        };
        let port = self.u16()?;
        Ok(PeerRecord {
            key,
            addr: SocketAddr::new(ip, port),
            seq: self.u64()?,
            capabilities: self.u32()?,
            signature: self.array()?,
        })
    }
}

/// The largest chunk payload: a Noise message less the chunk header.
pub const MAX_CHUNK_DATA: usize = MAX_MESSAGE_LEN - CHUNK_HEADER_LEN - START_HEADER_LEN;
/// Chunk header: one flags byte.
pub const CHUNK_HEADER_LEN: usize = 1;
/// After a start chunk's flags: the message type and the total body length.
pub const START_HEADER_LEN: usize = 1 + 4;
/// Chunk flag: the chunk belongs to the bulk stream (else the priority stream).
pub const FLAG_BULK: u8 = 0x80;
/// Chunk flag: the first chunk of a message; a type byte and the body length follow.
pub const FLAG_START: u8 = 0x01;
/// Chunk flag: the last chunk of a message.
pub const FLAG_END: u8 = 0x02;

/// Splits an encoded message into chunks of at most one Noise message each.
pub fn chunk(type_byte: u8, body: &[u8], bulk: bool, mut emit: impl FnMut(Vec<u8>)) {
    let stream = if bulk { FLAG_BULK } else { 0 };
    let mut offset = 0;
    let mut first = true;
    loop {
        let take = (body.len() - offset).min(MAX_CHUNK_DATA);
        let last = offset + take == body.len();
        let mut flags = stream;
        let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + START_HEADER_LEN + take);
        if first {
            flags |= FLAG_START;
        }
        if last {
            flags |= FLAG_END;
        }
        out.push(flags);
        if first {
            out.push(type_byte);
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        }
        out.extend_from_slice(&body[offset..offset + take]);
        emit(out);
        offset += take;
        first = false;
        if last {
            return;
        }
    }
}

/// Why a chunk sequence was refused. Every variant ends the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError {
    /// A chunk without a flags byte, or a start chunk without its header.
    Short,
    /// A continuation with no message started, or a start in the middle of one.
    OutOfOrder,
    /// The declared body length is above the limit for its message type.
    TooLarge {
        /// The message type.
        type_byte: u8,
        /// The declared length.
        declared: u32,
    },
    /// The chunks' data does not add up to the declared length.
    LengthMismatch,
    /// Unknown flag bits.
    BadFlags,
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ChunkError {}

#[derive(Debug, Default)]
struct Partial {
    type_byte: u8,
    declared: usize,
    body: Vec<u8>,
}

/// Reassembles messages from chunks, one in progress per stream. The declared length is checked
/// against `limit(type_byte)` before any buffer is grown, and a buffer grows only with data that
/// arrived, so a peer cannot make it allocate memory it did not send.
#[derive(Debug, Default)]
pub struct Reassembler {
    streams: [Option<Partial>; 2],
}

impl Reassembler {
    /// Feeds one chunk; returns a complete `(type_byte, body)` when this chunk ends a message.
    pub fn push(
        &mut self,
        chunk: &[u8],
        limit: impl Fn(u8) -> usize,
    ) -> Result<Option<(u8, Vec<u8>)>, ChunkError> {
        let (&flags, mut data) = chunk.split_first().ok_or(ChunkError::Short)?;
        if flags & !(FLAG_BULK | FLAG_START | FLAG_END) != 0 {
            return Err(ChunkError::BadFlags);
        }
        let slot = &mut self.streams[usize::from(flags & FLAG_BULK != 0)];
        if flags & FLAG_START != 0 {
            if slot.is_some() {
                return Err(ChunkError::OutOfOrder);
            }
            if data.len() < START_HEADER_LEN {
                return Err(ChunkError::Short);
            }
            let type_byte = data[0];
            let declared = u32::from_be_bytes(data[1..5].try_into().expect("4 bytes"));
            if declared as usize > limit(type_byte) {
                return Err(ChunkError::TooLarge { type_byte, declared });
            }
            data = &data[START_HEADER_LEN..];
            *slot = Some(Partial {
                type_byte,
                declared: declared as usize,
                body: Vec::with_capacity((declared as usize).min(MAX_CHUNK_DATA)),
            });
        }
        let partial = slot.as_mut().ok_or(ChunkError::OutOfOrder)?;
        if partial.body.len() + data.len() > partial.declared {
            return Err(ChunkError::LengthMismatch);
        }
        partial.body.extend_from_slice(data);
        if flags & FLAG_END == 0 {
            return Ok(None);
        }
        let partial = slot.take().expect("checked above");
        if partial.body.len() != partial.declared {
            return Err(ChunkError::LengthMismatch);
        }
        Ok(Some((partial.type_byte, partial.body)))
    }

    /// Whether a message is partly received (for the slow-sender deadline).
    pub fn in_progress(&self) -> bool {
        self.streams.iter().any(Option::is_some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretKey {
        SecretKey::from_byte_array([byte; 32]).unwrap()
    }

    fn samples() -> Vec<Message> {
        let net = [7u8; 32];
        vec![
            Message::Status(Status {
                version: 1,
                capabilities: 5,
                head_number: 9,
                head_hash: [3; 32],
                listen_port: 30400,
            }),
            Message::Ping(1),
            Message::Pong(u64::MAX),
            Message::Disconnect(DisconnectReason::Misbehaving),
            Message::GetPeers(16),
            Message::Peers(vec![
                PeerRecord::signed(&key(1), &net, "10.0.0.1:1".parse().unwrap(), 3, 1),
                PeerRecord::signed(&key(2), &net, "[::1]:65535".parse().unwrap(), 0, 0),
            ]),
            Message::Request { id: 7, kind: 2, body: vec![1, 2, 3] },
            Message::Response { id: 7, code: ResponseCode::Ok, body: vec![] },
            Message::Response { id: 8, code: ResponseCode::Busy, body: vec![] },
            Message::Gossip { topic: 3, data: vec![0; 100] },
            Message::IHave(vec![[1; 20], [2; 20]]),
            Message::IWant(vec![]),
            Message::IDontWant(vec![[4; 20]]),
            Message::Notify { kind: 1, body: vec![9; 40] },
        ]
    }

    #[test]
    fn round_trip_and_every_strict_prefix_fails() {
        for message in samples() {
            let body = message.encode_body();
            assert_eq!(Message::decode(message.type_byte(), &body), Ok(message.clone()));
            // A message whose last field takes the rest has valid shorter forms; the others must
            // refuse every truncation and an extra byte.
            let open_ended = matches!(
                message,
                Message::Request { .. }
                    | Message::Response { .. }
                    | Message::Gossip { .. }
                    | Message::Notify { .. }
            );
            if !open_ended {
                for len in 0..body.len() {
                    assert!(
                        Message::decode(message.type_byte(), &body[..len]).is_err(),
                        "{message:?} prefix {len}"
                    );
                }
                let mut long = body.clone();
                long.push(0);
                assert_eq!(
                    Message::decode(message.type_byte(), &long),
                    Err(WireError::TrailingBytes)
                );
            }
        }
    }

    #[test]
    fn huge_counts_fail_before_allocating() {
        let mut body = (u32::MAX).to_be_bytes().to_vec();
        assert_eq!(Message::decode(ty::IHAVE, &body), Err(WireError::TooMany));
        body = (MAX_IDS as u32).to_be_bytes().to_vec();
        assert_eq!(Message::decode(ty::IWANT, &body), Err(WireError::Truncated));
        assert_eq!(Message::decode(0x7f, &[]), Err(WireError::UnknownType(0x7f)));
        assert!(Message::decode(ty::RESPONSE, &[0, 0, 0, 1, 2, 9]).is_err());
    }

    #[test]
    fn records_are_bound_to_their_network() {
        let net = [7u8; 32];
        let mut record = PeerRecord::signed(&key(1), &net, "1.2.3.4:5".parse().unwrap(), 1, 0);
        assert!(record.verify(&net));
        assert!(!record.verify(&[8u8; 32]));
        record.seq = 2;
        assert!(!record.verify(&net));
    }

    #[test]
    fn chunking_reassembles_large_messages_on_both_streams() {
        let limit = |_| 4 * 1024 * 1024;
        for size in
            [0, 1, MAX_CHUNK_DATA - 1, MAX_CHUNK_DATA, MAX_CHUNK_DATA + 1, 3 * MAX_CHUNK_DATA + 17]
        {
            let body: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut bulk = Vec::new();
            chunk(ty::RESPONSE, &body, true, |c| bulk.push(c));
            let mut prio = Vec::new();
            chunk(ty::GOSSIP, &[5; 10], false, |c| prio.push(c));
            assert!(bulk.iter().all(|c| c.len() <= MAX_MESSAGE_LEN));
            let mut r = Reassembler::default();
            let mut done = Vec::new();
            // Interleave: the priority message lands in the middle of the bulk one.
            let mid = bulk.len() / 2;
            for c in bulk[..mid].iter().chain(prio.iter()).chain(bulk[mid..].iter()) {
                if let Some(m) = r.push(c, limit).unwrap() {
                    done.push(m);
                }
            }
            let bulk_done = done.iter().find(|(t, _)| *t == ty::RESPONSE).unwrap();
            assert_eq!(bulk_done.1, body);
            assert!(done.iter().any(|(t, b)| *t == ty::GOSSIP && b == &vec![5; 10]));
            assert!(!r.in_progress());
        }
    }

    #[test]
    fn reassembler_refuses_bad_sequences() {
        let limit = |_| 100usize;
        let mut r = Reassembler::default();
        assert_eq!(r.push(&[FLAG_END, 1], limit), Err(ChunkError::OutOfOrder));
        assert_eq!(
            r.push(&[FLAG_START, 1, 0, 0, 0, 101], limit),
            Err(ChunkError::TooLarge { type_byte: 1, declared: 101 })
        );
        let mut r = Reassembler::default();
        assert_eq!(r.push(&[FLAG_START, 1, 0, 0, 0, 2, 9], limit), Ok(None));
        assert_eq!(r.push(&[FLAG_START, 1, 0, 0, 0, 2], limit), Err(ChunkError::OutOfOrder));
        let mut r = Reassembler::default();
        assert_eq!(
            r.push(&[FLAG_START | FLAG_END, 1, 0, 0, 0, 2, 9], limit),
            Err(ChunkError::LengthMismatch)
        );
        assert_eq!(Reassembler::default().push(&[0x40], limit), Err(ChunkError::BadFlags));
        assert_eq!(Reassembler::default().push(&[FLAG_START, 1, 0], limit), Err(ChunkError::Short));
        let mut r = Reassembler::default();
        assert_eq!(
            r.push(&[FLAG_START, 1, 0, 0, 0, 1, 7, 7], limit),
            Err(ChunkError::LengthMismatch)
        );
    }
}
