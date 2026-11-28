//! One connected peer: the session thread that reads its messages, serves its requests from the
//! node's store, and routes responses to whoever is waiting for them.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::{Decodable, Header as RlpHeader};

use crate::{
    block::BlockBody,
    crypto::PeerId,
    eth::{
        self, BlockId, HeadersRequest, MAX_BODIES_SERVE, MAX_HEADERS_SERVE, SOFT_RESPONSE_LIMIT,
        Status, encode_response, receipt_to_eth68, receipt_to_eth69, split_request_id,
    },
    forkid::{ForkSchedule, Head},
    rlpx::{
        DisconnectReason, P2pMessage, Received, RlpxConnection, RlpxError, RlpxReader, RlpxWriter,
        SharedCapability,
    },
};

/// The node's chain, which the network serves to peers. Implemented by the node over its store.
pub trait BlockSource: Send + Sync {
    /// The current head and the earliest block served.
    fn chain_head(&self) -> ChainHead;
    /// The header with this hash, canonical or not.
    fn header_by_hash(&self, hash: B256) -> Option<Header>;
    /// The canonical header at this number.
    fn header_by_number(&self, number: u64) -> Option<Header>;
    /// The body of the block with this hash.
    fn body_by_hash(&self, hash: B256) -> Option<BlockBody>;
    /// The receipts of the block with this hash, each in its EIP-2718 encoding (the form the
    /// receipts trie hashes: `type || rlp([status, gas, bloom, logs, ..])`, legacy bare).
    fn receipts_by_hash(&self, hash: B256) -> Option<Vec<Bytes>>;
}

/// The node's head as the network announces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChainHead {
    /// Head block number.
    pub number: u64,
    /// Head block hash.
    pub hash: B256,
    /// Head block timestamp (decides the fork id).
    pub timestamp: u64,
    /// The lowest block whose body and receipts are served (eth/69 Status).
    pub earliest: u64,
}

/// What a waiting requester gets back.
#[derive(Debug)]
pub struct Response {
    /// The tag the requester chose.
    pub tag: u64,
    /// The peer that answered.
    pub peer: PeerId,
    /// The response payload (including the request id), or `None` if the peer went away.
    pub payload: Option<Vec<u8>>,
}

struct Pending {
    response_id: u8,
    tag: u64,
    tx: Sender<Response>,
}

/// A connected peer, shared between its session thread and requesters.
pub struct Peer {
    /// The peer's node id.
    pub id: PeerId,
    /// The remote address.
    pub addr: SocketAddr,
    /// Whether the peer dialed us.
    pub inbound: bool,
    /// Whether the peer is configured as trusted (never limited, always redialed).
    pub trusted: bool,
    /// The client name from Hello.
    pub client_id: String,
    eth: SharedCapability,
    head: Mutex<(u64, B256)>,
    writer: Mutex<RlpxWriter>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_request_id: AtomicU64,
    inflight: AtomicUsize,
    closed: AtomicBool,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer").field("id", &self.id).field("addr", &self.addr).finish()
    }
}

/// Why a request could not be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// The peer is gone.
    Closed,
}

impl Peer {
    /// The negotiated eth version.
    pub fn eth_version(&self) -> u64 {
        self.eth.version
    }

    /// The peer's last announced head (number is 0 for eth/68 peers, which do not announce it).
    pub fn head(&self) -> (u64, B256) {
        *self.head.lock().unwrap()
    }

    /// Requests in flight on this peer.
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Whether the session has ended.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Sends an eth request with relative id `request_id`; the response with relative id
    /// `request_id + 1` is delivered to `tx` with `tag`. `encode` builds the payload from the
    /// request id.
    pub fn request_eth(
        &self,
        request_id: u8,
        encode: impl FnOnce(u64) -> Vec<u8>,
        tag: u64,
        tx: &Sender<Response>,
    ) -> Result<(), RequestError> {
        if self.is_closed() {
            return Err(RequestError::Closed);
        }
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let pending =
            Pending { response_id: self.eth.offset + request_id + 1, tag, tx: tx.clone() };
        self.pending.lock().unwrap().insert(id, pending);
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let payload = encode(id);
        if self.writer.lock().unwrap().send(self.eth.offset + request_id, &payload).is_err() {
            self.cancel(id);
            self.close(DisconnectReason::TcpError);
            return Err(RequestError::Closed);
        }
        Ok(())
    }

    /// Drops a request whose response is no longer wanted (after a timeout).
    pub fn cancel(&self, request_id: u64) {
        if self.pending.lock().unwrap().remove(&request_id).is_some() {
            self.inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Cancels every pending request carrying `tag` (the requester stopped waiting).
    pub fn cancel_tag(&self, tag: u64) {
        let mut pending = self.pending.lock().unwrap();
        let before = pending.len();
        pending.retain(|_, p| p.tag != tag);
        self.inflight.fetch_sub(before - pending.len(), Ordering::Relaxed);
    }

    /// Disconnects the peer with `reason`; the session thread then exits.
    pub fn close(&self, reason: DisconnectReason) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            let _ = self.writer.lock().unwrap().disconnect(reason);
        }
    }

    fn send(&self, id: u8, payload: &[u8]) -> Result<(), RlpxError> {
        self.writer.lock().unwrap().send(id, payload)
    }

    fn deliver(&self, message_id: u8, payload: Vec<u8>) -> Result<(), &'static str> {
        let (request_id, _) =
            split_request_id(&payload).map_err(|_| "response without request id")?;
        let pending = self.pending.lock().unwrap().remove(&request_id);
        let Some(pending) = pending else {
            // A response after we stopped waiting (timeout); harmless.
            return Ok(());
        };
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        if pending.response_id != message_id {
            return Err("response of the wrong type");
        }
        let _ =
            pending.tx.send(Response { tag: pending.tag, peer: self.id, payload: Some(payload) });
        Ok(())
    }

    fn fail_pending(&self) {
        let pending: Vec<Pending> = self.pending.lock().unwrap().drain().map(|(_, p)| p).collect();
        self.inflight.store(0, Ordering::Relaxed);
        for p in pending {
            let _ = p.tx.send(Response { tag: p.tag, peer: self.id, payload: None });
        }
    }
}

/// Session settings fixed by the network.
#[derive(Debug, Clone)]
pub(crate) struct SessionConfig {
    /// The network id (chain id).
    pub network_id: u64,
    /// The fork schedule, for Status and peer fork id checks.
    pub forks: ForkSchedule,
    /// How long the Status exchange may take.
    pub handshake_timeout: Duration,
    /// Ping interval; a peer that misses two is dropped.
    pub ping_interval: Duration,
}

/// The local Status for `version` from the chain head.
pub(crate) fn local_status(config: &SessionConfig, version: u64, head: ChainHead) -> Status {
    Status {
        version,
        network_id: config.network_id,
        genesis: config.forks.genesis_hash,
        fork_id: config.forks.fork_id(Head { number: head.number, timestamp: head.timestamp }),
        head_hash: head.hash,
        earliest: head.earliest,
        latest: head.number,
    }
}

/// Why a session ended before or after the Status exchange.
#[derive(Debug)]
pub(crate) enum SessionError {
    /// Transport failure.
    Rlpx(RlpxError),
    /// The peer is on another chain or fork, or broke the protocol: worth a ban.
    Incompatible,
    /// Protocol breach after the handshake: worth a ban.
    Breach,
}

impl From<RlpxError> for SessionError {
    fn from(error: RlpxError) -> Self {
        Self::Rlpx(error)
    }
}

/// Exchanges Status on a fresh connection and checks the peer's chain and fork.
pub(crate) fn handshake(
    mut conn: RlpxConnection,
    config: &SessionConfig,
    chain: &dyn BlockSource,
    trusted: bool,
    inbound: bool,
) -> Result<(Arc<Peer>, RlpxReader), SessionError> {
    let Some(eth) = conn.shared.iter().find(|c| c.name == "eth").cloned() else {
        let _ = conn.writer.disconnect(DisconnectReason::UselessPeer);
        return Err(SessionError::Incompatible /* no eth capability */);
    };
    let head = chain.chain_head();
    let ours = local_status(config, eth.version, head);
    conn.writer.send(eth.offset + eth::STATUS, &ours.encode_payload())?;
    let deadline = Instant::now() + config.handshake_timeout;
    conn.reader.set_timeout(Duration::from_millis(500))?;
    let theirs = loop {
        if Instant::now() > deadline {
            return Err(SessionError::Rlpx(RlpxError::Io(std::io::ErrorKind::TimedOut.into())));
        }
        let message = match conn.reader.poll() {
            Ok(Received::Message(message)) => message,
            Ok(Received::Idle) => continue,
            // base-reth's eth handshake disconnects with ProtocolBreach on an oversized message.
            Err(RlpxError::MessageTooBig) => {
                let _ = conn.writer.disconnect(DisconnectReason::ProtocolBreach);
                return Err(SessionError::Rlpx(RlpxError::MessageTooBig));
            }
            Err(error) => return Err(error.into()),
        };
        match P2pMessage::decode(&message)? {
            Some(P2pMessage::Ping) => {
                conn.writer.pong()?;
                continue;
            }
            Some(P2pMessage::Pong) => continue,
            Some(P2pMessage::Disconnect(reason)) => {
                return Err(SessionError::Rlpx(RlpxError::Disconnected(reason)));
            }
            None => {}
        }
        // base-reth's eth handshake disconnects with ProtocolBreach on anything but a valid
        // Status, and on every Status mismatch below.
        let status = (message.id == eth.offset + eth::STATUS)
            .then(|| Status::decode_payload(eth.version, &message.payload).ok())
            .flatten();
        match status {
            Some(status) => break status,
            None => {
                let _ = conn.writer.disconnect(DisconnectReason::ProtocolBreach);
                return Err(SessionError::Incompatible /* not a valid status */);
            }
        }
    };
    let reject = |writer: &mut RlpxWriter, _why: &'static str| {
        let _ = writer.disconnect(DisconnectReason::ProtocolBreach);
        Err(SessionError::Incompatible)
    };
    if theirs.network_id != config.network_id {
        return reject(&mut conn.writer, "network id");
    }
    if theirs.genesis != config.forks.genesis_hash {
        return reject(&mut conn.writer, "genesis");
    }
    let local_head = Head { number: head.number, timestamp: head.timestamp };
    if config.forks.validate(local_head, theirs.fork_id).is_err() {
        return reject(&mut conn.writer, "fork id");
    }
    // base-reth fails these eth/69 checks without a Disconnect, so the connection just closes.
    if theirs.version >= 69 && (theirs.earliest > theirs.latest || theirs.head_hash.is_zero()) {
        return Err(SessionError::Incompatible /* bad block range */);
    }
    let peer = Arc::new(Peer {
        id: conn.remote_id,
        addr: conn.writer.peer_addr().unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
        inbound,
        trusted,
        client_id: conn.remote_hello.client_id.clone(),
        eth,
        head: Mutex::new((theirs.latest, theirs.head_hash)),
        writer: Mutex::new(conn.writer),
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        inflight: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
    });
    Ok((peer, conn.reader))
}

/// Runs the session loop until the peer disconnects or misbehaves. Returns why it ended.
pub(crate) fn run_session(
    peer: &Peer,
    mut reader: RlpxReader,
    config: &SessionConfig,
    chain: &dyn BlockSource,
    shutdown: &AtomicBool,
) -> SessionError {
    let error = session_loop(peer, &mut reader, config, chain, shutdown);
    peer.close(match &error {
        // Malformed or oversized messages are a protocol breach to base-reth too.
        SessionError::Breach
        | SessionError::Incompatible
        | SessionError::Rlpx(RlpxError::Protocol(_) | RlpxError::MessageTooBig) => {
            DisconnectReason::ProtocolBreach
        }
        SessionError::Rlpx(RlpxError::Disconnected(_)) => DisconnectReason::Requested,
        SessionError::Rlpx(_) => DisconnectReason::TcpError,
    });
    peer.fail_pending();
    error
}

fn session_loop(
    peer: &Peer,
    reader: &mut RlpxReader,
    config: &SessionConfig,
    chain: &dyn BlockSource,
    shutdown: &AtomicBool,
) -> SessionError {
    if let Err(error) = reader.set_timeout(Duration::from_millis(500)) {
        return error.into();
    }
    let mut last_ping = Instant::now();
    let mut awaiting_pong: Option<Instant> = None;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            peer.close(DisconnectReason::ClientQuitting);
        }
        if peer.is_closed() {
            return SessionError::Rlpx(RlpxError::Disconnected(DisconnectReason::Requested as u8));
        }
        if let Some(sent) = awaiting_pong
            && sent.elapsed() > config.ping_interval * 2
        {
            peer.close(DisconnectReason::PingTimeout);
            return SessionError::Rlpx(RlpxError::Protocol("ping timeout"));
        }
        if awaiting_pong.is_none() && last_ping.elapsed() > config.ping_interval {
            if let Err(error) = peer.writer.lock().unwrap().ping() {
                return error.into();
            }
            last_ping = Instant::now();
            awaiting_pong = Some(last_ping);
        }
        let message = match reader.poll() {
            Ok(Received::Idle) => continue,
            Ok(Received::Message(message)) => message,
            Err(error) => return error.into(),
        };
        let result = match P2pMessage::decode(&message) {
            Err(error) => return error.into(),
            Ok(Some(P2pMessage::Ping)) => peer.writer.lock().unwrap().pong().map_err(Into::into),
            Ok(Some(P2pMessage::Pong)) => {
                awaiting_pong = None;
                Ok(())
            }
            Ok(Some(P2pMessage::Disconnect(reason))) => {
                return SessionError::Rlpx(RlpxError::Disconnected(reason));
            }
            Ok(None) => {
                let id = message.id;
                if id >= peer.eth.offset && id < peer.eth.offset + peer.eth.count {
                    handle_eth(peer, id - peer.eth.offset, message.payload, chain)
                } else {
                    Err(SessionError::Breach /* message id outside negotiated range */)
                }
            }
        };
        if let Err(error) = result {
            return error;
        }
    }
}

fn handle_eth(
    peer: &Peer,
    id: u8,
    payload: Vec<u8>,
    chain: &dyn BlockSource,
) -> Result<(), SessionError> {
    let offset = peer.eth.offset;
    let breach = |_| SessionError::Breach /* bad eth message */;
    match id {
        eth::BLOCK_HEADERS | eth::BLOCK_BODIES | eth::RECEIPTS | eth::POOLED_TRANSACTIONS => {
            peer.deliver(offset + id, payload).map_err(|_| SessionError::Breach)
        }
        eth::GET_BLOCK_HEADERS => {
            let (request_id, mut body) = split_request_id(&payload).map_err(breach)?;
            let request = HeadersRequest::decode(&mut body).map_err(breach)?;
            let headers = serve_headers(chain, &request);
            peer.send(offset + eth::BLOCK_HEADERS, &encode_response(request_id, &headers))?;
            Ok(())
        }
        eth::GET_BLOCK_BODIES => {
            let (request_id, mut body) = split_request_id(&payload).map_err(breach)?;
            let hashes = Vec::<B256>::decode(&mut body).map_err(breach)?;
            let mut bodies = Vec::new();
            let mut bytes = 0;
            for hash in hashes.iter().take(MAX_BODIES_SERVE) {
                let Some(block) = chain.body_by_hash(*hash) else { break };
                let encoded = alloy_rlp::encode(&block);
                bytes += encoded.len();
                bodies.push(encoded);
                if bytes > SOFT_RESPONSE_LIMIT {
                    break;
                }
            }
            peer.send(offset + eth::BLOCK_BODIES, &encode_response(request_id, &bodies))?;
            Ok(())
        }
        eth::GET_RECEIPTS => {
            let (request_id, mut body) = split_request_id(&payload).map_err(breach)?;
            let hashes = Vec::<B256>::decode(&mut body).map_err(breach)?;
            let mut lists = Vec::new();
            let mut bytes = 0;
            for hash in hashes.iter().take(MAX_BODIES_SERVE) {
                let Some(receipts) = chain.receipts_by_hash(*hash) else { break };
                let mut items = Vec::with_capacity(receipts.len());
                for receipt in &receipts {
                    items.push(if peer.eth.version >= 69 {
                        receipt_to_eth69(receipt)
                            .map_err(|_| SessionError::Breach /* bad stored receipt */)?
                    } else {
                        receipt_to_eth68(receipt)
                    });
                }
                let encoded = eth::encode_list_of_raw(&items);
                bytes += encoded.len();
                lists.push(encoded);
                if bytes > SOFT_RESPONSE_LIMIT {
                    break;
                }
            }
            peer.send(offset + eth::RECEIPTS, &encode_response(request_id, &lists))?;
            Ok(())
        }
        eth::GET_POOLED_TRANSACTIONS => {
            // Tiger keeps no transaction pool: answer with an empty list.
            let (request_id, _) = split_request_id(&payload).map_err(breach)?;
            peer.send(
                offset + eth::POOLED_TRANSACTIONS,
                &encode_response::<&[u8]>(request_id, &[]),
            )?;
            Ok(())
        }
        eth::BLOCK_RANGE_UPDATE => {
            let mut buf = payload.as_slice();
            let mut items = RlpHeader::decode_bytes(&mut buf, true).map_err(breach)?;
            let _earliest = u64::decode(&mut items).map_err(breach)?;
            let latest = u64::decode(&mut items).map_err(breach)?;
            let hash = B256::decode(&mut items).map_err(breach)?;
            *peer.head.lock().unwrap() = (latest, hash);
            Ok(())
        }
        // Announcements and broadcasts: base-reth disables transaction gossip and block
        // propagation, and tiger has no pool, so these are accepted and dropped.
        eth::NEW_BLOCK_HASHES
        | eth::TRANSACTIONS
        | eth::NEW_BLOCK
        | eth::NEW_POOLED_TRANSACTION_HASHES => Ok(()),
        eth::STATUS => Err(SessionError::Breach /* second status */),
        _ => Ok(()),
    }
}

/// Collects encoded headers for a GetBlockHeaders request.
fn serve_headers(chain: &dyn BlockSource, request: &HeadersRequest) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let limit = request.limit.min(MAX_HEADERS_SERVE as u64);
    if limit == 0 {
        return out;
    }
    let first = match request.start {
        BlockId::Hash(hash) => chain.header_by_hash(hash),
        BlockId::Number(number) => chain.header_by_number(number),
    };
    let Some(first) = first else { return out };
    let step = request.skip.saturating_add(1);
    let mut number = first.number;
    let mut header = Some(first);
    let mut bytes = 0;
    while let Some(current) = header {
        let encoded = alloy_rlp::encode(&current);
        bytes += encoded.len();
        out.push(encoded);
        if out.len() as u64 >= limit || bytes > SOFT_RESPONSE_LIMIT {
            break;
        }
        number = if request.reverse {
            match number.checked_sub(step) {
                Some(n) => n,
                None => break,
            }
        } else {
            match number.checked_add(step) {
                Some(n) => n,
                None => break,
            }
        };
        header = chain.header_by_number(number);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{peer_id_of_secret, random_secret_key},
        eth::ETH_CAPABILITIES,
        forkid::ForkSchedule,
    };
    use std::{net::TcpListener, thread};

    struct Chain(Vec<Header>);

    fn session_config() -> SessionConfig {
        SessionConfig {
            network_id: 8453,
            forks: ForkSchedule::base(B256::ZERO, 0, 0, &[]),
            handshake_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(60),
        }
    }

    fn test_chain() -> Chain {
        Chain((0..4).map(|number| Header { number, ..Default::default() }).collect())
    }

    /// Starts a server that runs `handshake` and then `run_session` on one connection, and
    /// returns the remote end of that connection before the Status exchange, with the eth
    /// capability's offset.
    fn connect_before_status() -> (RlpxConnection, u8) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_key = random_secret_key();
        let server_id = peer_id_of_secret(&server_key);
        let timeout = Duration::from_secs(5);
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let conn =
                RlpxConnection::accept(stream, &server_key, &ETH_CAPABILITIES, "tiger", timeout)
                    .unwrap();
            let (config, chain) = (session_config(), test_chain());
            let Ok((peer, reader)) = handshake(conn, &config, &chain, false, true) else { return };
            run_session(&peer, reader, &config, &chain, &AtomicBool::new(false));
        });
        let key = random_secret_key();
        let conn =
            RlpxConnection::connect(addr, server_id, &key, &ETH_CAPABILITIES, "remote", timeout)
                .unwrap();
        let offset = conn.shared[0].offset;
        (conn, offset)
    }

    /// Like [`connect_before_status`], after the Status exchange.
    fn connect_to_session() -> (RlpxConnection, u8) {
        let (mut conn, offset) = connect_before_status();
        let status = local_status(&session_config(), 69, test_chain().chain_head());
        conn.writer.send(offset + eth::STATUS, &status.encode_payload()).unwrap();
        loop {
            let Received::Message(message) = conn.reader.poll().unwrap() else { continue };
            if message.id == offset + eth::STATUS {
                break;
            }
        }
        (conn, offset)
    }

    /// Reads until the session disconnects or goes quiet for a second: the Pongs received and
    /// the Disconnect reason, if any.
    fn read_pongs_until_disconnect(conn: &mut RlpxConnection) -> (usize, Option<u8>) {
        conn.reader.set_timeout(Duration::from_secs(1)).unwrap();
        let mut pongs = 0;
        loop {
            let message = match conn.reader.poll() {
                Ok(Received::Message(message)) => message,
                Ok(Received::Idle) | Err(_) => return (pongs, None),
            };
            match P2pMessage::decode(&message) {
                Ok(Some(P2pMessage::Pong)) => pongs += 1,
                Ok(Some(P2pMessage::Disconnect(reason))) => return (pongs, Some(reason)),
                _ => {}
            }
        }
    }

    impl BlockSource for Chain {
        fn chain_head(&self) -> ChainHead {
            let last = self.0.last().unwrap();
            ChainHead { number: last.number, hash: last.hash_slow(), timestamp: 0, earliest: 0 }
        }
        fn header_by_hash(&self, hash: B256) -> Option<Header> {
            self.0.iter().find(|h| h.hash_slow() == hash).cloned()
        }
        fn header_by_number(&self, number: u64) -> Option<Header> {
            self.0.get(number as usize).cloned()
        }
        fn body_by_hash(&self, _: B256) -> Option<BlockBody> {
            None
        }
        fn receipts_by_hash(&self, _: B256) -> Option<Vec<Bytes>> {
            None
        }
    }

    #[test]
    fn serves_header_ranges_with_skip_and_reverse() {
        let chain = Chain((0..20).map(|number| Header { number, ..Default::default() }).collect());
        let numbers = |request: HeadersRequest| -> Vec<u64> {
            serve_headers(&chain, &request)
                .iter()
                .map(|h| Header::decode(&mut h.as_slice()).unwrap().number)
                .collect()
        };
        let forward =
            HeadersRequest { start: BlockId::Number(3), limit: 4, skip: 2, reverse: false };
        assert_eq!(numbers(forward), vec![3, 6, 9, 12]);
        let back = HeadersRequest {
            start: BlockId::Hash(chain.0[5].hash_slow()),
            limit: 10,
            skip: 0,
            reverse: true,
        };
        assert_eq!(numbers(back), vec![5, 4, 3, 2, 1, 0]);
        let past_end =
            HeadersRequest { start: BlockId::Number(18), limit: 5, skip: 0, reverse: false };
        assert_eq!(numbers(past_end), vec![18, 19]);
    }

    #[test]
    fn limit_zero_serves_no_headers() {
        let chain = Chain((0..4).map(|number| Header { number, ..Default::default() }).collect());
        // GetBlockHeaders [7, [start 0, limit 0, skip 0, reverse 0]].
        let payload = alloy_primitives::hex::decode("c607c480808080").unwrap();
        let (request_id, mut body) = split_request_id(&payload).unwrap();
        let request = HeadersRequest::decode(&mut body).unwrap();
        assert_eq!((request_id, request.limit), (7, 0));
        assert!(serve_headers(&chain, &request).is_empty());
    }

    #[test]
    fn eth_message_over_10_mib_disconnects_with_protocol_breach() {
        // A 9 MiB broadcast is within the limit: ignored, and the session still answers a Ping.
        let (mut conn, offset) = connect_to_session();
        let broadcast = alloy_rlp::encode(vec![0u8; 9 * 1024 * 1024]);
        conn.writer.send(offset + eth::TRANSACTIONS, &broadcast).unwrap();
        conn.writer.ping().unwrap();
        assert_eq!(read_pongs_until_disconnect(&mut conn), (1, None));
        // An 11 MiB one is refused from its Snappy length prefix.
        let (mut conn, offset) = connect_to_session();
        let broadcast = alloy_rlp::encode(vec![0u8; 11 * 1024 * 1024]);
        conn.writer.send(offset + eth::TRANSACTIONS, &broadcast).unwrap();
        let breach = Some(DisconnectReason::ProtocolBreach as u8);
        assert_eq!(read_pongs_until_disconnect(&mut conn), (0, breach));
    }

    #[test]
    fn eth_message_over_10_mib_before_status_disconnects_with_protocol_breach() {
        let (mut conn, offset) = connect_before_status();
        let broadcast = alloy_rlp::encode(vec![0u8; 11 * 1024 * 1024]);
        conn.writer.send(offset + eth::TRANSACTIONS, &broadcast).unwrap();
        let breach = Some(DisconnectReason::ProtocolBreach as u8);
        assert_eq!(read_pongs_until_disconnect(&mut conn), (0, breach));
        // A Ping is answered; a small message other than Status is a breach too.
        let (mut conn, offset) = connect_before_status();
        conn.writer.ping().unwrap();
        conn.writer.send(offset + eth::TRANSACTIONS, &alloy_rlp::encode(vec![0u8; 8])).unwrap();
        assert_eq!(read_pongs_until_disconnect(&mut conn), (1, breach));
    }

    #[test]
    fn bad_status_contents_end_the_handshake_like_base_reth() {
        let status = local_status(&session_config(), 69, test_chain().chain_head());
        // An extra field fails to decode: ProtocolBreach.
        let (mut conn, offset) = connect_before_status();
        let encoded = status.encode_payload();
        let mut fields = RlpHeader::decode_bytes(&mut &encoded[..], true).unwrap().to_vec();
        fields.push(0x80);
        let payload = crate::eth::encode_list_of_raw(&[fields]);
        conn.writer.send(offset + eth::STATUS, &payload).unwrap();
        let breach = Some(DisconnectReason::ProtocolBreach as u8);
        assert_eq!(read_pongs_until_disconnect(&mut conn), (0, breach));
        // An eth/69 range with earliest above latest: closed without a Disconnect.
        let (mut conn, offset) = connect_before_status();
        let bad = Status { earliest: status.latest + 1, ..status };
        conn.writer.send(offset + eth::STATUS, &bad.encode_payload()).unwrap();
        assert_eq!(read_pongs_until_disconnect(&mut conn), (0, None));
    }

    #[test]
    fn ping_flood_and_bad_ping_payload_disconnect_with_protocol_breach() {
        let breach = Some(DisconnectReason::ProtocolBreach as u8);
        // 50 back-to-back Pings: five Pongs, then a disconnect.
        let (mut conn, _) = connect_to_session();
        for _ in 0..50 {
            // Later writes may find the socket already closed.
            let _ = conn.writer.ping();
        }
        assert_eq!(read_pongs_until_disconnect(&mut conn), (5, breach));
        // A Ping whose payload is `c180` instead of `c0`.
        let (mut conn, _) = connect_to_session();
        conn.writer.send(0x02, &[0xc1, 0x80]).unwrap();
        assert_eq!(read_pongs_until_disconnect(&mut conn), (0, breach));
    }
}
