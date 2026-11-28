//! The scripted `RLPx` peer: the harness's own handshake and frame codec (wire.rs) over TCP, so a
//! scenario decides every byte of every frame, including headers, compression and message ids.

use std::{
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    time::{Duration, Instant},
};

use alloy_primitives::hex;
use secp256k1::{PublicKey, SECP256K1, SecretKey};

use crate::el::{
    rlp::{self, Item, Node, b, int, l},
    rng::Rng,
    wire::{FrameReader, FrameWriter, Handshake, Seal, node_id, public_of_id, seal_packet},
};

/// A target node: name, `RLPx` address and node id.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Display name.
    pub(crate) name: String,
    /// `RLPx` address.
    pub(crate) addr: SocketAddr,
    /// 64-byte node id.
    pub(crate) id: [u8; 64],
}

impl Target {
    /// Parses `name=enode://<id>@<ip>:<port>`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let (name, enode) = text.split_once('=').ok_or("expected name=enode://id@ip:port")?;
        let rest = enode.strip_prefix("enode://").ok_or("expected enode://")?;
        let (id, addr) = rest.split_once('@').ok_or("expected id@ip:port")?;
        let addr = addr.split('?').next().unwrap();
        let id: [u8; 64] = hex::decode(id)
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "node id must be 64 bytes")?;
        Ok(Self {
            name: name.to_string(),
            addr: addr.parse().map_err(|e| format!("{addr}: {e}"))?,
            id,
        })
    }
}

/// Something the peer saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// A message (absolute id, payload after decompression).
    Message(u8, Vec<u8>),
    /// A message whose payload did not decompress.
    Garbled(u8, Vec<u8>),
    /// Disconnect with the reason byte (or the raw payload when it does not parse).
    Disconnect(String),
    /// The socket closed or failed.
    Closed(String),
    /// Nothing arrived in time.
    Timeout,
}

/// A connected peer after the ECIES handshake.
pub(crate) struct Peer {
    stream: TcpStream,
    /// Frame encryption towards the node.
    pub(crate) writer: FrameWriter,
    reader: FrameReader,
    buf: Vec<u8>,
    pending: Option<usize>,
    /// Compress messages after Hello.
    pub(crate) snappy: bool,
    /// Our node key.
    pub(crate) key: SecretKey,
    /// Pings the node sent (answered automatically).
    pub(crate) pings: u32,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer").field("snappy", &self.snappy).finish()
    }
}

/// The reason name for a disconnect code.
pub(crate) fn reason_name(code: u8) -> String {
    let name = match code {
        0x00 => "Requested",
        0x01 => "TcpError",
        0x02 => "ProtocolBreach",
        0x03 => "UselessPeer",
        0x04 => "TooManyPeers",
        0x05 => "AlreadyConnected",
        0x06 => "IncompatibleVersion",
        0x07 => "NullIdentity",
        0x08 => "ClientQuitting",
        0x09 => "UnexpectedIdentity",
        0x0a => "ConnectedToSelf",
        0x0b => "PingTimeout",
        0x10 => "SubprotocolSpecific",
        _ => return format!("0x{code:02x}"),
    };
    name.to_string()
}

fn parse_reason(payload: &[u8]) -> String {
    match rlp::parse(payload) {
        Some((node, _)) => match node.item {
            Item::List(items) => match items.first().map(|n| &n.item) {
                Some(Item::Bytes(b)) if b.len() <= 1 => {
                    reason_name(b.first().copied().unwrap_or(0))
                }
                _ => format!("raw:{}", hex::encode(payload)),
            },
            Item::Bytes(b) if b.len() <= 1 => reason_name(b.first().copied().unwrap_or(0)),
            Item::Bytes(_) => format!("raw:{}", hex::encode(payload)),
        },
        None => format!("raw:{}", hex::encode(payload)),
    }
}

impl Peer {
    /// Dials `target` from `source` and runs the handshake with a key from `rng`.
    pub(crate) fn connect(
        target: &Target,
        source: Ipv4Addr,
        rng: &mut Rng,
    ) -> Result<Self, String> {
        let key = crate::el::wire::secret(rng);
        Self::connect_with(target, source, rng, key, Seal::default())
    }

    /// Dials with a given key and handshake packet framing.
    pub(crate) fn connect_with(
        target: &Target,
        source: Ipv4Addr,
        rng: &mut Rng,
        key: SecretKey,
        seal: Seal,
    ) -> Result<Self, String> {
        let remote = public_of_id(&target.id).ok_or("bad node id")?;
        let mut stream = dial(target.addr, source)?;
        let hs = Handshake::new(rng, key);
        let auth = seal_packet(rng, &remote, &hs.auth_body(&remote, 4).encode(), seal);
        stream.write_all(&auth).map_err(|e| format!("write auth: {e}"))?;
        let ack = read_packet(&mut stream)?;
        let (writer, reader) = hs.finish(&auth, &ack).ok_or("bad ack")?;
        Ok(Self {
            stream,
            writer,
            reader,
            buf: Vec::new(),
            pending: None,
            snappy: false,
            key,
            pings: 0,
        })
    }

    /// Our node id.
    pub(crate) fn id(&self) -> [u8; 64] {
        node_id(&PublicKey::from_secret_key(SECP256K1, &self.key))
    }

    /// Writes raw bytes to the socket.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write_all(bytes).map_err(|e| format!("write: {e}"))
    }

    /// Sends one frame carrying `frame` (message id and data, already encoded).
    pub(crate) fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        let bytes = self.writer.frame(frame);
        self.write_raw(&bytes)
    }

    /// Sends message `id` with RLP `payload`, Snappy-compressed when the session uses it.
    pub(crate) fn send(&mut self, id: u8, payload: &[u8]) -> Result<(), String> {
        let data = if self.snappy {
            snap::raw::Encoder::new().compress_vec(payload).unwrap()
        } else {
            payload.to_vec()
        };
        self.send_data(id, &data)
    }

    /// Sends message `id` with `data` exactly as given (no compression).
    pub(crate) fn send_data(&mut self, id: u8, data: &[u8]) -> Result<(), String> {
        let mut frame = alloy_rlp::encode(id);
        frame.extend_from_slice(data);
        self.send_frame(&frame)
    }

    fn fill(&mut self, n: usize, deadline: Instant) -> Result<bool, String> {
        let mut chunk = [0u8; 64 * 1024];
        while self.buf.len() < n {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            self.stream.set_read_timeout(Some((deadline - now).max(Duration::from_millis(1)))).ok();
            let want = (n - self.buf.len()).min(chunk.len());
            match self.stream.read(&mut chunk[..want]) {
                Ok(0) => return Err("eof".into()),
                Ok(k) => self.buf.extend_from_slice(&chunk[..k]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(false);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.kind().to_string()),
            }
        }
        Ok(true)
    }

    /// Reads the next frame payload (message id and data) within `timeout`.
    pub(crate) fn recv_frame(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        let deadline = Instant::now() + timeout;
        let size = match self.pending {
            Some(size) => size,
            None => {
                if !self.fill(32, deadline)? {
                    return Ok(None);
                }
                let header: [u8; 32] = self.buf[..32].try_into().unwrap();
                self.buf.drain(..32);
                let size = self.reader.header(&header).ok_or("bad header mac")?;
                self.pending = Some(size);
                size
            }
        };
        let need = size.next_multiple_of(16) + 16;
        if !self.fill(need, deadline)? {
            return Ok(None);
        }
        self.pending = None;
        let body: Vec<u8> = self.buf.drain(..need).collect();
        Ok(Some(self.reader.body(&body, size).ok_or("bad body mac")?))
    }

    /// The next event, answering the node's Pings on the way.
    pub(crate) fn recv(&mut self, timeout: Duration) -> Event {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let frame = match self.recv_frame(left) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Event::Timeout,
                Err(e) => return Event::Closed(e),
            };
            let Some((id_node, used)) = rlp::parse(&frame) else {
                return Event::Garbled(0xff, frame);
            };
            let id = match id_node.item {
                Item::Bytes(b) if b.len() <= 1 => b.first().copied().unwrap_or(0),
                _ => return Event::Garbled(0xff, frame),
            };
            let data = &frame[used..];
            let payload = if self.snappy && id != 0 {
                match snap::raw::Decoder::new().decompress_vec(data) {
                    Ok(p) => p,
                    Err(_) if id == 1 => data.to_vec(),
                    Err(_) => return Event::Garbled(id, data.to_vec()),
                }
            } else {
                data.to_vec()
            };
            match id {
                0x01 => return Event::Disconnect(parse_reason(&payload)),
                0x02 => {
                    self.pings += 1;
                    let _ = self.send(0x03, &[0xc0]);
                }
                _ => return Event::Message(id, payload),
            }
        }
    }

    /// Reads events for `window` (or until the connection ends).
    pub(crate) fn observe(&mut self, window: Duration) -> Vec<Event> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.recv(left) {
                Event::Timeout => break,
                e @ (Event::Closed(_) | Event::Disconnect(_)) => {
                    let closing = matches!(e, Event::Disconnect(_));
                    out.push(e);
                    if closing {
                        // The socket should close right after the Disconnect.
                        if let Event::Closed(c) = self.recv(Duration::from_millis(500)) {
                            out.push(Event::Closed(c));
                        }
                    }
                    break;
                }
                e => out.push(e),
            }
        }
        out
    }
}

fn dial(addr: SocketAddr, source: Ipv4Addr) -> Result<TcpStream, String> {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .map_err(|e| e.to_string())?;
    socket
        .bind(&SocketAddr::V4(SocketAddrV4::new(source, 0)).into())
        .map_err(|e| format!("bind {source}: {e}"))?;
    socket
        .connect_timeout(&addr.into(), Duration::from_secs(5))
        .map_err(|e| format!("connect: {e}"))?;
    let stream: TcpStream = socket.into();
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    Ok(stream)
}

fn read_packet(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).map_err(|e| format!("read ack: {}", e.kind()))?;
    let len = u16::from_be_bytes(prefix) as usize;
    let mut packet = vec![0u8; len + 2];
    packet[..2].copy_from_slice(&prefix);
    stream.read_exact(&mut packet[2..]).map_err(|e| format!("read ack: {}", e.kind()))?;
    Ok(packet)
}

/// A Hello message.
#[derive(Debug, Clone)]
pub(crate) struct HelloSpec {
    /// p2p version.
    pub(crate) version: u64,
    /// Capabilities `(name, version)`.
    pub(crate) caps: Vec<(&'static str, u64)>,
    /// Extra trailing list elements (EIP-8).
    pub(crate) extra: Vec<Node>,
    /// Client id.
    pub(crate) client: Vec<u8>,
    /// Node id override (default: ours).
    pub(crate) id: Option<Vec<u8>>,
}

impl HelloSpec {
    /// The default: p2p v5, `eth/version`.
    pub(crate) fn eth(version: u64) -> Self {
        Self {
            version: 5,
            caps: vec![("eth", version)],
            extra: Vec::new(),
            client: b"tiger-p2p-fuzz".to_vec(),
            id: None,
        }
    }

    /// The Hello payload for `peer`.
    pub(crate) fn encode(&self, peer: &Peer) -> Vec<u8> {
        let caps =
            self.caps.iter().map(|(n, v)| l(vec![b(n.as_bytes().to_vec()), int(*v)])).collect();
        let id = self.id.clone().unwrap_or_else(|| peer.id().to_vec());
        let mut fields = vec![int(self.version), b(self.client.clone()), l(caps), int(0), b(id)];
        fields.extend(self.extra.iter().cloned());
        l(fields).encode()
    }
}

/// Exchanges Hello: sends ours, reads theirs. Returns their Hello payload or what happened.
pub(crate) fn hello(peer: &mut Peer, spec: &HelloSpec) -> Result<Vec<u8>, Vec<Event>> {
    let payload = spec.encode(peer);
    if let Err(e) = peer.send_data(0x00, &payload) {
        return Err(vec![Event::Closed(e)]);
    }
    match peer.recv(Duration::from_secs(5)) {
        Event::Message(0x00, theirs) => {
            let their_version = rlp::parse(&theirs)
                .and_then(|(n, _)| match n.item {
                    Item::List(items) => match items.first().map(|i| &i.item) {
                        Some(Item::Bytes(b)) => {
                            Some(b.iter().fold(0u64, |a, x| (a << 8) | *x as u64))
                        }
                        _ => None,
                    },
                    Item::Bytes(_) => None,
                })
                .unwrap_or(0);
            peer.snappy = spec.version >= 5 && their_version >= 5;
            Ok(theirs)
        }
        other => {
            let mut events = vec![other];
            events.extend(peer.observe(Duration::from_millis(500)));
            Err(events)
        }
    }
}
