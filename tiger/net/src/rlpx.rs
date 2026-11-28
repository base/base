//! The RLPx connection: TCP plus the ECIES handshake, the devp2p `p2p` capability (Hello,
//! Disconnect, Ping, Pong), capability negotiation, and Snappy message compression.
//!
//! A connection is split into a [`RlpxReader`] and a [`RlpxWriter`] over clones of one socket, so
//! a session's reader thread can block on the socket while other threads send requests.

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use alloy_rlp::{Decodable, Encodable, Header, RlpDecodable, RlpEncodable};
use secp256k1::SecretKey;

use crate::{
    crypto::{PeerId, peer_id_of_secret},
    ecies::{self, EciesError, FrameCodec, FrameEgress, FrameIngress, Initiator},
    snappy::{self, SnappyError},
};

/// The devp2p base protocol version tiger speaks (v5 adds Snappy compression).
pub(crate) const P2P_VERSION: u64 = 5;
/// Largest uncompressed message accepted, as in reth and geth.
pub(crate) const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
/// Largest eth message accepted, base-reth's `MAX_MESSAGE_SIZE`. reth counts the message id
/// byte, so the payload may be one byte less. Every capability tiger negotiates is eth.
const MAX_ETH_MESSAGE_SIZE: usize = 10 * 1024 * 1024;
/// Message ids below this are reserved for the `p2p` capability.
pub(crate) const BASE_PROTOCOL_OFFSET: u8 = 0x10;

/// `p2p` message ids.
const HELLO: u8 = 0x00;
const DISCONNECT: u8 = 0x01;
const PING: u8 = 0x02;
const PONG: u8 = 0x03;

/// Errors of an RLPx connection. Every one ends the connection.
#[derive(Debug)]
pub enum RlpxError {
    /// Socket error, including timeouts during the handshake.
    Io(io::Error),
    /// Handshake or frame authentication failure.
    Ecies(EciesError),
    /// A message that does not decode, or a protocol violation.
    Protocol(&'static str),
    /// The remote disconnected with this reason code.
    Disconnected(u8),
    /// No capability in common.
    NoSharedCapability,
}

impl From<io::Error> for RlpxError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<EciesError> for RlpxError {
    fn from(error: EciesError) -> Self {
        Self::Ecies(error)
    }
}

impl From<alloy_rlp::Error> for RlpxError {
    fn from(_: alloy_rlp::Error) -> Self {
        Self::Protocol("bad rlp")
    }
}

/// Disconnect reason codes from the devp2p spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DisconnectReason {
    /// Disconnect requested.
    Requested = 0x00,
    /// TCP subsystem error.
    TcpError = 0x01,
    /// Breach of protocol, for example a malformed message or bad response.
    ProtocolBreach = 0x02,
    /// Useless peer: wrong chain, fork, or no shared capability.
    UselessPeer = 0x03,
    /// Too many peers.
    TooManyPeers = 0x04,
    /// Already connected.
    AlreadyConnected = 0x05,
    /// Incompatible p2p protocol version.
    IncompatibleVersion = 0x06,
    /// Client quitting.
    ClientQuitting = 0x08,
    /// The peer did not respond in time.
    PingTimeout = 0x0b,
    /// Some other reason specific to a subprotocol.
    SubprotocolSpecific = 0x10,
}

/// A capability a node supports: name and version, such as `eth/69`.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Capability {
    /// The capability name.
    pub name: String,
    /// The capability version.
    pub version: u64,
}

/// The `p2p` Hello message.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable)]
pub struct Hello {
    /// The highest devp2p base protocol version supported.
    pub protocol_version: u64,
    /// A free-form client name.
    pub client_id: String,
    /// The supported capabilities.
    pub capabilities: Vec<Capability>,
    /// The listening port (unused by modern clients; 0 is allowed).
    pub port: u16,
    /// The sender's node id.
    pub id: PeerId,
}

impl Decodable for Hello {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // EIP-8: ignore extra trailing list elements.
        let mut items = Header::decode_bytes(buf, true)?;
        Ok(Self {
            protocol_version: u64::decode(&mut items)?,
            client_id: String::decode(&mut items)?,
            capabilities: Vec::<Capability>::decode(&mut items)?,
            port: u16::decode(&mut items)?,
            id: PeerId::decode(&mut items)?,
        })
    }
}

/// A capability both sides share, with its message id range on this connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedCapability {
    /// The capability name.
    pub name: String,
    /// The highest version both sides support.
    pub version: u64,
    /// The first message id of this capability on this connection.
    pub offset: u8,
    /// How many message ids the capability uses.
    pub count: u8,
}

/// What a local capability is and how many message ids each version uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCapability {
    /// The capability name.
    pub name: &'static str,
    /// The version.
    pub version: u64,
    /// Message ids this version uses.
    pub count: u8,
}

/// Computes the shared capabilities: the highest common version of each name, ordered by name,
/// with message ids assigned consecutively from 0x10 (devp2p rules).
pub(crate) fn negotiate(
    local: &[LocalCapability],
    remote: &[Capability],
) -> Result<Vec<SharedCapability>, RlpxError> {
    let mut best: Vec<LocalCapability> = Vec::new();
    for cap in remote {
        let Some(ours) = local.iter().find(|l| l.name == cap.name && l.version == cap.version)
        else {
            continue;
        };
        match best.iter_mut().find(|b| b.name == ours.name) {
            Some(existing) if existing.version < ours.version => *existing = *ours,
            Some(_) => {}
            None => best.push(*ours),
        }
    }
    if best.is_empty() {
        return Err(RlpxError::NoSharedCapability);
    }
    best.sort_by(|a, b| a.name.cmp(b.name));
    let mut offset = BASE_PROTOCOL_OFFSET;
    let mut shared = Vec::with_capacity(best.len());
    for cap in best {
        shared.push(SharedCapability {
            name: cap.name.to_string(),
            version: cap.version,
            offset,
            count: cap.count,
        });
        offset = offset.checked_add(cap.count).ok_or(RlpxError::Protocol("too many messages"))?;
    }
    Ok(shared)
}

/// A message received on a connection, with the absolute message id.
#[derive(Debug)]
pub struct RawMessage {
    /// The absolute message id (capability offset plus the message's id within it).
    pub id: u8,
    /// The RLP payload, decompressed.
    pub payload: Vec<u8>,
}

/// The receiving half of an RLPx connection.
#[derive(Debug)]
pub struct RlpxReader {
    stream: TcpStream,
    ingress: FrameIngress,
    snappy: bool,
    buf: Vec<u8>,
    /// Payload size of the frame whose header was read, waiting for its body.
    pending: Option<usize>,
}

/// The sending half of an RLPx connection.
#[derive(Debug)]
pub struct RlpxWriter {
    stream: TcpStream,
    egress: FrameEgress,
    snappy: bool,
    buf: Vec<u8>,
}

/// An established connection, after Hello.
#[derive(Debug)]
pub struct RlpxConnection {
    /// The receiving half.
    pub reader: RlpxReader,
    /// The sending half.
    pub writer: RlpxWriter,
    /// The remote's node id (authenticated by the handshake).
    pub remote_id: PeerId,
    /// The remote's Hello.
    pub remote_hello: Hello,
    /// The negotiated capabilities.
    pub shared: Vec<SharedCapability>,
}

fn read_handshake_packet(stream: &mut TcpStream) -> Result<Vec<u8>, RlpxError> {
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix)?;
    let len = ecies::handshake_packet_len(prefix)?;
    let mut packet = vec![0u8; len];
    packet[..2].copy_from_slice(&prefix);
    stream.read_exact(&mut packet[2..])?;
    Ok(packet)
}

impl RlpxConnection {
    /// Dials `addr`, runs the handshake against the expected `remote_id`, and exchanges Hello.
    pub fn connect(
        addr: SocketAddr,
        remote_id: PeerId,
        key: &SecretKey,
        local_caps: &[LocalCapability],
        client_id: &str,
        timeout: Duration,
    ) -> Result<Self, RlpxError> {
        let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let initiator = Initiator::new(key, &remote_id)?;
        stream.write_all(initiator.auth_packet())?;
        let ack = read_handshake_packet(&mut stream)?;
        let codec = initiator.finish(key, &ack)?;
        Self::hello(stream, codec, remote_id, key, local_caps, client_id)
    }

    /// Runs the recipient side of the handshake on an accepted socket and exchanges Hello.
    pub fn accept(
        mut stream: TcpStream,
        key: &SecretKey,
        local_caps: &[LocalCapability],
        client_id: &str,
        timeout: Duration,
    ) -> Result<Self, RlpxError> {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let auth = read_handshake_packet(&mut stream)?;
        let accepted = ecies::accept(key, &auth)?;
        stream.write_all(&accepted.ack)?;
        Self::hello(stream, accepted.codec, accepted.remote, key, local_caps, client_id)
    }

    fn hello(
        stream: TcpStream,
        codec: FrameCodec,
        remote_id: PeerId,
        key: &SecretKey,
        local_caps: &[LocalCapability],
        client_id: &str,
    ) -> Result<Self, RlpxError> {
        let mut writer = RlpxWriter {
            stream: stream.try_clone()?,
            egress: codec.egress,
            snappy: false,
            buf: Vec::new(),
        };
        let mut reader = RlpxReader {
            stream,
            ingress: codec.ingress,
            snappy: false,
            buf: Vec::new(),
            pending: None,
        };
        let hello = Hello {
            protocol_version: P2P_VERSION,
            client_id: client_id.to_string(),
            capabilities: local_caps
                .iter()
                .map(|c| Capability { name: c.name.to_string(), version: c.version })
                .collect(),
            port: 0,
            id: peer_id_of_secret(key),
        };
        writer.send(HELLO, &alloy_rlp::encode(&hello))?;
        let message = reader.recv()?;
        match message.id {
            HELLO => {}
            DISCONNECT => return Err(RlpxError::Disconnected(disconnect_reason(&message.payload))),
            _ => return Err(RlpxError::Protocol("expected hello")),
        }
        let remote_hello = Hello::decode(&mut message.payload.as_slice())?;
        if remote_hello.id != remote_id {
            return Err(RlpxError::Protocol("hello id differs from handshake id"));
        }
        let shared = match negotiate(local_caps, &remote_hello.capabilities) {
            Ok(shared) => shared,
            Err(error) => {
                let _ = writer.disconnect(DisconnectReason::UselessPeer);
                return Err(error);
            }
        };
        let snappy = remote_hello.protocol_version >= P2P_VERSION;
        writer.snappy = snappy;
        reader.snappy = snappy;
        Ok(Self { reader, writer, remote_id, remote_hello, shared })
    }
}

/// Decodes the reason of a Disconnect payload: `[reason]`, or a bare byte some clients send.
fn disconnect_reason(payload: &[u8]) -> u8 {
    let mut buf = payload;
    if let Ok(reasons) = Vec::<u8>::decode(&mut buf) {
        return reasons.first().copied().unwrap_or(0);
    }
    let mut buf = payload;
    u8::decode(&mut buf).unwrap_or(0)
}

impl RlpxWriter {
    /// Sends one message with absolute id `id` and RLP `payload`.
    pub fn send(&mut self, id: u8, payload: &[u8]) -> Result<(), RlpxError> {
        assert!(payload.len() <= MAX_MESSAGE_SIZE);
        let mut frame = Vec::with_capacity(payload.len() + 8);
        id.encode(&mut frame);
        if self.snappy {
            frame.extend_from_slice(&snappy::compress(payload));
        } else {
            frame.extend_from_slice(payload);
        }
        self.buf.clear();
        self.egress.encode(&frame, &mut self.buf);
        self.stream.write_all(&self.buf)?;
        Ok(())
    }

    /// Sends a Ping.
    pub fn ping(&mut self) -> Result<(), RlpxError> {
        self.send(PING, &[0xc0])
    }

    /// Sends a Pong.
    pub fn pong(&mut self) -> Result<(), RlpxError> {
        self.send(PONG, &[0xc0])
    }

    /// Sends Disconnect and shuts the socket down.
    pub fn disconnect(&mut self, reason: DisconnectReason) -> Result<(), RlpxError> {
        let result = self.send(DISCONNECT, &alloy_rlp::encode(vec![reason as u8]));
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        result
    }

    /// The remote socket address.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.stream.peer_addr().ok()
    }
}

/// What [`RlpxReader::poll`] produced.
#[derive(Debug)]
pub enum Received {
    /// A subprotocol message.
    Message(RawMessage),
    /// The read timeout passed without a complete message; call `poll` again.
    Idle,
}

impl RlpxReader {
    /// Sets the socket read timeout that bounds how long [`Self::poll`] blocks.
    pub fn set_timeout(&self, timeout: Duration) -> Result<(), RlpxError> {
        self.stream.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    /// Reads until `self.buf` holds `n` bytes. A timeout keeps the partial data for the next call.
    fn fill(&mut self, n: usize) -> Result<bool, RlpxError> {
        let mut chunk = [0u8; 16 * 1024];
        while self.buf.len() < n {
            let want = (n - self.buf.len()).min(chunk.len());
            match self.stream.read(&mut chunk[..want]) {
                Ok(0) => return Err(RlpxError::Io(io::ErrorKind::UnexpectedEof.into())),
                Ok(read) => self.buf.extend_from_slice(&chunk[..read]),
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    return Ok(false);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(true)
    }

    /// Reads the next frame, waiting at most the socket read timeout for it to complete.
    /// Answers nothing itself: Ping, Pong and Disconnect come back as messages with ids 1..3.
    pub fn poll(&mut self) -> Result<Received, RlpxError> {
        let size = match self.pending {
            Some(size) => size,
            None => {
                if !self.fill(32)? {
                    return Ok(Received::Idle);
                }
                let mut header: [u8; 32] = self.buf[..32].try_into().unwrap();
                self.buf.clear();
                let size = self.ingress.decode_header(&mut header)?;
                self.pending = Some(size);
                size
            }
        };
        if !self.fill(FrameCodec::body_len(size))? {
            return Ok(Received::Idle);
        }
        self.pending = None;
        let mut body = std::mem::take(&mut self.buf);
        let frame = self.ingress.decode_body(&mut body, size)?;
        let mut cursor = frame;
        let id = u8::decode(&mut cursor)?;
        let max =
            if id >= BASE_PROTOCOL_OFFSET { MAX_ETH_MESSAGE_SIZE - 1 } else { MAX_MESSAGE_SIZE };
        // The Snappy length prefix is checked against `max` before anything is allocated.
        let payload = if self.snappy {
            snappy::decompress(cursor, max).map_err(|error| match error {
                SnappyError::TooLarge => RlpxError::Protocol("message too big"),
                SnappyError::Truncated | SnappyError::Corrupt => RlpxError::Protocol("bad snappy"),
            })?
        } else if cursor.len() > max {
            return Err(RlpxError::Protocol("message too big"));
        } else {
            cursor.to_vec()
        };
        Ok(Received::Message(RawMessage { id, payload }))
    }

    /// Blocks until a full message arrives (used during the handshake, under its timeout).
    fn recv(&mut self) -> Result<RawMessage, RlpxError> {
        match self.poll()? {
            Received::Message(message) => Ok(message),
            Received::Idle => Err(RlpxError::Io(io::ErrorKind::TimedOut.into())),
        }
    }
}

/// Classifies a `p2p` message id: returns `Some` for the base protocol messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P2pMessage {
    /// Remote disconnects with a reason.
    Disconnect(u8),
    /// Remote pings; answer with Pong.
    Ping,
    /// Remote answered our Ping.
    Pong,
}

impl P2pMessage {
    /// Decodes a base protocol message, or `None` if `message` belongs to a capability.
    pub fn decode(message: &RawMessage) -> Result<Option<Self>, RlpxError> {
        Ok(match message.id {
            DISCONNECT => Some(Self::Disconnect(disconnect_reason(&message.payload))),
            PING => Some(Self::Ping),
            PONG => Some(Self::Pong),
            HELLO => return Err(RlpxError::Protocol("second hello")),
            id if id < BASE_PROTOCOL_OFFSET => return Err(RlpxError::Protocol("reserved id")),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::random_secret_key;
    use alloy_primitives::hex;
    use std::net::TcpListener;

    const CAPS: &[LocalCapability] = &[
        LocalCapability { name: "eth", version: 68, count: 17 },
        LocalCapability { name: "eth", version: 69, count: 18 },
        LocalCapability { name: "snap", version: 2, count: 10 },
    ];

    #[test]
    fn decodes_eip8_hello_with_extra_elements() {
        let bytes = hex::decode(
            "f87137916b6e6574682f76302e39312f706c616e39cdc5836574683dc6846d6f726b1682270fb840\
             fda1cff674c90c9a197539fe3dfb53086ace64f83ed7c6eabec741f7f381cc803e52ab2cd55d5569\
             bce4347107a310dfd5f88a010cd2ffd1005ca406f1842877c883666f6f836261720304",
        )
        .unwrap();
        let hello = Hello::decode(&mut bytes.as_slice()).unwrap();
        assert_eq!(hello.protocol_version, 55);
        assert_eq!(hello.client_id, "kneth/v0.91/plan9");
        assert_eq!(hello.capabilities[0], Capability { name: "eth".into(), version: 61 });
        assert_eq!(hello.port, 9999);
    }

    #[test]
    fn negotiation_picks_highest_versions_in_name_order() {
        let remote = vec![
            Capability { name: "snap".into(), version: 2 },
            Capability { name: "eth".into(), version: 66 },
            Capability { name: "eth".into(), version: 68 },
            Capability { name: "eth".into(), version: 69 },
            Capability { name: "eth".into(), version: 71 },
        ];
        let shared = negotiate(CAPS, &remote).unwrap();
        assert_eq!(shared[0].name, "eth");
        assert_eq!((shared[0].version, shared[0].offset), (69, 0x10));
        assert_eq!((shared[1].name.as_str(), shared[1].offset), ("snap", 0x10 + 18));
        let none = negotiate(CAPS, &[Capability { name: "les".into(), version: 4 }]);
        assert!(matches!(none, Err(RlpxError::NoSharedCapability)));
    }

    #[test]
    fn connects_and_exchanges_messages_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_key = random_secret_key();
        let server_id = peer_id_of_secret(&server_key);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut conn =
                RlpxConnection::accept(stream, &server_key, CAPS, "server", Duration::from_secs(5))
                    .unwrap();
            let Received::Message(m) = conn.reader.poll().unwrap() else { panic!("idle") };
            conn.writer.send(m.id, &m.payload).unwrap();
        });
        let client_key = random_secret_key();
        let mut conn = RlpxConnection::connect(
            addr,
            server_id,
            &client_key,
            CAPS,
            "client",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(conn.remote_hello.client_id, "server");
        assert_eq!(conn.shared.len(), 2);
        let payload = alloy_rlp::encode(vec![7u64; 1000]);
        conn.writer.send(0x13, &payload).unwrap();
        let Received::Message(m) = conn.reader.poll().unwrap() else { panic!("idle") };
        assert_eq!((m.id, m.payload), (0x13, payload));
        server.join().unwrap();
    }
}
