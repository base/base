//! tp2p for the tiger node (Cargo feature `tp2p`, off by default): blocks, headers and execution
//! witnesses from other tiger nodes over `base-tiger-p2p`, alongside devp2p or instead of it.
//!
//! What it serves:
//! - headers (`GET_HEADERS`, eth's `GetBlockHeaders` fields) and bodies (`GET_BODIES`) from the
//!   chain, as the devp2p server does;
//! - with `--tp2p.serve-witness`, the execution witness of each recent block the node imported
//!   (`GET_WITNESS`), built from the keys its validation read ([`crate::WitnessCache`]). The
//!   node announces each one (`ANNOUNCE`), so stateless peers fetch it before their consensus node
//!   even sends the block.
//!
//! What it uses: a stateless node fetches witnesses from peers first and from its upstream RPC
//! only when no peer has one ([`crate::Witnesses`]). EL sync downloads through [`Fetcher`].
//! Everything a peer sends goes through the same checks as from any other source: headers
//! link by hash, bodies match their header's roots, and witness nodes are verified by hash
//! against tiger's own parent state root, so a peer can withhold data but not inject it.

use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread,
    time::Duration,
};

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use base_tiger_net::{BlockBody, BlockId, BlockSource, DownloadError, Downloader, HeadersRequest, SealedHeader};
use base_tiger_p2p::{
    Config, Event, Handle, Network, NotifyKind, PeerId, Penalty, RequestError, RequestId, RequestKind,
    ResponseCode, Target,
};
use secp256k1::{PublicKey, SecretKey};

use crate::{ExecutionWitness, Node, Upstream};

/// Request kind: headers, body `HeadersRequest` RLP as in eth (request id 0), answer an RLP list.
pub const GET_HEADERS: u16 = 1;
/// Request kind: bodies, body an RLP list of block hashes, answer an RLP list of bodies.
pub const GET_BODIES: u16 = 2;
/// Request kind: a witness, body `hash (32) || number (8, big-endian)`, answer
/// [`crate::encode_witness`].
pub const GET_WITNESS: u16 = 3;
/// Notice kind: a block's witness is ready, body `hash (32) || number (8)`.
pub const ANNOUNCE: u16 = 1;
/// Capability: serves headers and bodies.
pub const CAP_BLOCKS: u32 = 1;
/// Capability: serves witnesses.
pub const CAP_WITNESS: u32 = 2;

const MAX_HEADERS: u64 = 1024;
const MAX_BODIES: usize = 128;
/// The largest headers and bodies answers, before and after snappy (both travel compressed, as
/// devp2p's do).
const MAX_HEADERS_BYTES: usize = 4 << 20;
const MAX_BODIES_BYTES: usize = 32 << 20;
const SERVE_WORKERS: usize = 4;
const WITNESS_WAIT: Duration = Duration::from_secs(2);
const WITNESS_ATTEMPTS: usize = 3;
const DOWNLOAD_ATTEMPTS: usize = 4;

/// The request kinds and limits both sides configure.
fn kinds() -> Vec<RequestKind> {
    vec![
        RequestKind { kind: GET_HEADERS, capability: CAP_BLOCKS, max_request: 64, max_response: MAX_HEADERS_BYTES, per_peer_per_second: 50, timeout_ms: 5_000 },
        RequestKind { kind: GET_BODIES, capability: CAP_BLOCKS, max_request: 8 + MAX_BODIES * 33, max_response: MAX_BODIES_BYTES, per_peer_per_second: 50, timeout_ms: 20_000 },
        RequestKind { kind: GET_WITNESS, capability: CAP_WITNESS, max_request: 40, max_response: 64 << 20, per_peer_per_second: 20, timeout_ms: 10_000 },
    ]
}

/// `--tp2p.*` options.
#[derive(Debug, Clone)]
pub struct Tp2pArgs {
    /// Where to listen.
    pub listen: SocketAddr,
    /// The address to advertise, when it differs from `listen`.
    pub advertise: Option<SocketAddr>,
    /// Peers to dial first.
    pub bootnodes: Vec<(PublicKey, SocketAddr)>,
    /// Peers never banned.
    pub trusted: Vec<PublicKey>,
    /// Whether to build and serve witnesses (a full node).
    pub serve_witness: bool,
    /// The node key.
    pub secret: SecretKey,
}

/// Parses `tp2p://<66 hex compressed key>@host:port`.
pub fn parse_peer(text: &str) -> Result<(PublicKey, SocketAddr), String> {
    let rest = text.strip_prefix("tp2p://").ok_or_else(|| format!("{text}: missing tp2p:// prefix"))?;
    let (key, host) = rest.split_once('@').ok_or_else(|| format!("{text}: missing @"))?;
    let bytes = alloy_primitives::hex::decode(key).map_err(|error| format!("{text}: {error}"))?;
    let key = PublicKey::from_slice(&bytes).map_err(|error| format!("{text}: {error}"))?;
    let addr = host.to_socket_addrs().map_err(|error| format!("{text}: {error}"))?.next().ok_or_else(|| format!("{text}: unresolvable"))?;
    Ok((key, addr))
}

/// A request's answer and the peer that gave it.
type Answer = (PeerId, Result<Vec<u8>, RequestError>);

#[derive(Debug, Default)]
struct Waiting {
    senders: HashMap<RequestId, mpsc::SyncSender<Answer>>,
    early: HashMap<RequestId, Answer>,
}

/// The running tp2p network of a tiger node.
#[derive(Debug)]
pub struct Tp2p {
    handle: Handle,
    waiting: Mutex<Waiting>,
}

impl Tp2p {
    /// The underlying handle (peers, traffic counters).
    pub const fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Sends a request and waits for its answer.
    pub fn request(&self, kind: u16, body: Vec<u8>, target: Target) -> Result<(PeerId, Vec<u8>), (PeerId, RequestError)> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = self.handle.request(kind, body, target);
        {
            let mut waiting = self.waiting.lock().expect("tp2p waiting");
            if let Some((peer, result)) = waiting.early.remove(&id) {
                return result.map(|body| (peer, body)).map_err(|error| (peer, error));
            }
            waiting.senders.insert(id, sender);
        }
        // The node always answers: with the body, a refusal, a timeout or a disconnect.
        let (peer, result) = receiver.recv().unwrap_or(([0; 33], Err(RequestError::Disconnected)));
        result.map(|body| (peer, body)).map_err(|error| (peer, error))
    }

    fn answered(&self, request: RequestId, peer: PeerId, result: Result<Vec<u8>, RequestError>) {
        let mut waiting = self.waiting.lock().expect("tp2p waiting");
        match waiting.senders.remove(&request) {
            Some(sender) => {
                let _ = sender.send((peer, result));
            }
            None => {
                if waiting.early.len() > 4096 {
                    waiting.early.clear();
                }
                waiting.early.insert(request, (peer, result));
            }
        }
    }

    /// The witness of block `hash` (number `number`) from a peer that serves witnesses, trying a
    /// few; the caller verifies it against its own state.
    pub fn fetch_witness(&self, hash: B256, number: u64) -> Result<ExecutionWitness, String> {
        let mut body = hash.to_vec();
        body.extend_from_slice(&number.to_be_bytes());
        let mut tried = Vec::new();
        let mut last = String::from("no peer serves witnesses");
        for _ in 0..WITNESS_ATTEMPTS {
            match self.request(GET_WITNESS, body.clone(), Target::AnyExcept(tried.clone())) {
                Ok((peer, bytes)) => match crate::decode_witness(&bytes) {
                    Ok(witness) => return Ok(witness),
                    Err(error) => {
                        self.handle.report(peer, Penalty::BadResponse);
                        tried.push(peer);
                        last = format!("undecodable witness: {error}");
                    }
                },
                Err((_, RequestError::NoPeer)) => break,
                Err((peer, error)) => {
                    tried.push(peer);
                    last = format!("{error:?}");
                }
            }
        }
        Err(format!("tp2p witness of block {number} {hash}: {last}"))
    }

    /// Headers as eth's `GetBlockHeaders` answers them, from one peer, checked: the first one is
    /// the one asked for, and with `skip` 0 each links to the previous.
    pub fn headers(&self, request: HeadersRequest) -> Result<Vec<SealedHeader>, String> {
        let mut tried = Vec::new();
        let mut last = String::from("no peer serves blocks");
        for _ in 0..DOWNLOAD_ATTEMPTS {
            let (peer, bytes) = match self.request(GET_HEADERS, request.encode_payload(0), Target::AnyExcept(tried.clone())) {
                Ok(answer) => answer,
                Err((_, RequestError::NoPeer)) => break,
                Err((peer, error)) => {
                    tried.push(peer);
                    last = format!("{error:?}");
                    continue;
                }
            };
            let checked = base_tiger_net::snappy_decompress(&bytes, MAX_HEADERS_BYTES)
                .map_err(|error| format!("{error:?}"))
                .and_then(|bytes| check_headers(&request, &bytes));
            match checked {
                Ok(headers) => return Ok(headers),
                Err(error) => {
                    self.handle.report(peer, Penalty::BadResponse);
                    tried.push(peer);
                    last = error;
                }
            }
        }
        Err(format!("tp2p headers: {last}"))
    }

    /// Headers from `target` back toward `stop_at`, descending and linked, at most `max`; the
    /// target is always first.
    pub fn headers_back(&self, target: B256, stop_at: u64, max: u64) -> Result<Vec<SealedHeader>, String> {
        let request = HeadersRequest { start: BlockId::Hash(target), limit: max.min(MAX_HEADERS), skip: 0, reverse: true };
        let mut headers = self.headers(request)?;
        // Like devp2p's downloader, the target itself always comes back (the answer starts there).
        let top = headers[0].number();
        headers.retain(|header| header.number() == top || header.number() >= stop_at);
        Ok(headers)
    }

    /// The bodies of `headers`, each matched to its header's roots, fetched in batches spread
    /// over peers in parallel.
    pub fn bodies(&self, headers: &[SealedHeader]) -> Result<Vec<BlockBody>, String> {
        let mut bodies: Vec<Option<BlockBody>> = vec![None; headers.len()];
        for _ in 0..DOWNLOAD_ATTEMPTS {
            let missing: Vec<usize> = (0..headers.len()).filter(|i| bodies[*i].is_none()).collect();
            if missing.is_empty() {
                break;
            }
            // Send every batch first, then collect: the node spreads them over peers.
            let mut inflight = Vec::new();
            for batch in missing.chunks(MAX_BODIES) {
                let hashes: Vec<B256> = batch.iter().map(|i| headers[*i].hash).collect();
                let (sender, receiver) = mpsc::sync_channel(1);
                let id = self.handle.request(GET_BODIES, alloy_rlp::encode(&hashes), Target::Any);
                let mut waiting = self.waiting.lock().expect("tp2p waiting");
                match waiting.early.remove(&id) {
                    Some(answer) => {
                        let _ = sender.send(answer);
                    }
                    None => {
                        waiting.senders.insert(id, sender);
                    }
                }
                drop(waiting);
                inflight.push((batch.to_vec(), receiver));
            }
            for (batch, receiver) in inflight {
                let Ok((peer, Ok(bytes))) = receiver.recv() else { continue };
                let decoded = base_tiger_net::snappy_decompress(&bytes, MAX_BODIES_BYTES).ok();
                let Some(Ok(list)) = decoded.map(|bytes| Vec::<BlockBody>::decode(&mut bytes.as_slice())) else {
                    self.handle.report(peer, Penalty::BadResponse);
                    continue;
                };
                if list.len() > batch.len() {
                    self.handle.report(peer, Penalty::BadResponse);
                    continue;
                }
                for (index, body) in batch.iter().zip(list) {
                    if body.validate(&headers[*index].header).is_ok() {
                        bodies[*index] = Some(body);
                    } else {
                        self.handle.report(peer, Penalty::BadResponse);
                        break;
                    }
                }
            }
        }
        bodies.into_iter().enumerate().map(|(i, body)| body.ok_or_else(|| format!("tp2p: no body for block {}", headers[i].number()))).collect()
    }
}

/// Where a stateless node's witnesses come from: tiger peers over tp2p first, once started, then
/// the upstream RPC.
#[derive(Debug, Clone, Default)]
pub struct WitnessSource {
    /// The upstream RPC.
    pub upstream: Option<Upstream>,
    /// tp2p, once the node starts it.
    pub tp2p: Arc<OnceLock<Arc<Tp2p>>>,
}

impl WitnessSource {
    /// The witness of block `number` `hash`.
    pub fn execution_witness(&self, hash: B256, number: u64, by_hash: bool) -> Result<ExecutionWitness, String> {
        if let Some(tp2p) = self.tp2p.get() {
            match tp2p.fetch_witness(hash, number) {
                Ok(witness) => return Ok(witness),
                Err(error) if self.upstream.is_none() => return Err(error),
                Err(_) => {}
            }
        }
        match &self.upstream {
            Some(upstream) => upstream.execution_witness(hash, number, by_hash),
            None => Err("no witness source".into()),
        }
    }
}

/// Where EL sync downloads blocks from: devp2p peers, or tiger peers over tp2p. Without the
/// feature, sync uses [`Downloader`] directly; this has its signatures.
#[derive(Debug)]
pub enum Fetcher {
    /// eth/69 over devp2p.
    Devp2p(Downloader),
    /// tp2p.
    Tp2p(Arc<Tp2p>),
}

impl Fetcher {
    /// Headers for `request`.
    pub fn headers(&self, request: HeadersRequest) -> Result<Vec<SealedHeader>, DownloadError> {
        match self {
            Self::Devp2p(downloader) => downloader.headers(request),
            Self::Tp2p(tp2p) => tp2p.headers(request).map_err(Self::failed),
        }
    }

    /// Headers from `target` back toward `stop_at`, descending and linked, at most `max`.
    pub fn headers_back(&self, target: B256, stop_at: u64, max: u64) -> Result<Vec<SealedHeader>, DownloadError> {
        match self {
            Self::Devp2p(downloader) => downloader.headers_back(target, stop_at, max),
            Self::Tp2p(tp2p) => tp2p.headers_back(target, stop_at, max).map_err(Self::failed),
        }
    }

    /// The bodies of `headers`, each checked against its header.
    pub fn bodies(&self, headers: &[SealedHeader], progress: &mut dyn FnMut(usize)) -> Result<Vec<BlockBody>, DownloadError> {
        match self {
            Self::Devp2p(downloader) => downloader.bodies(headers, progress),
            Self::Tp2p(tp2p) => tp2p.bodies(headers).map_err(Self::failed),
        }
    }

    /// A tp2p failure as the downloader's error (the detail goes to the log).
    fn failed(error: String) -> DownloadError {
        eprintln!("tiger: tp2p download: {error}");
        DownloadError::Exhausted("tp2p")
    }
}

/// Decodes and checks a headers answer against its request.
fn check_headers(request: &HeadersRequest, mut bytes: &[u8]) -> Result<Vec<SealedHeader>, String> {
    let header = alloy_rlp::Header::decode(&mut bytes).map_err(|error| error.to_string())?;
    if !header.list || header.payload_length != bytes.len() {
        return Err("headers answer is not one RLP list".into());
    }
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        headers.push(SealedHeader::decode(&mut bytes).map_err(|error| error.to_string())?);
    }
    if headers.len() as u64 > request.limit.min(MAX_HEADERS) {
        return Err("more headers than asked for".into());
    }
    let Some(first) = headers.first() else { return Err("no headers".into()) };
    let starts = match request.start {
        BlockId::Hash(hash) => first.hash == hash,
        BlockId::Number(number) => first.number() == number,
    };
    if !starts {
        return Err("headers start elsewhere".into());
    }
    if request.skip == 0 {
        let linked = headers.windows(2).all(|pair| {
            if request.reverse { pair[0].header.parent_hash == pair[1].hash } else { pair[1].header.parent_hash == pair[0].hash }
        });
        if !linked {
            return Err("headers do not link".into());
        }
    }
    Ok(headers)
}

impl Node {
    /// Starts tp2p (feature `tp2p`).
    pub fn start_tp2p(self: &Arc<Self>, args: Tp2pArgs) -> Result<Arc<Tp2p>, String> {
        let (genesis, chain_id) = {
            let chain = self.chain.read().expect("chain lock");
            (chain.genesis.hash, self.spec.chain_id)
        };
        let network = Network { chain_id, genesis: genesis.0, layer: Network::EXECUTION };
        let mut config = Config::new(network, args.secret, 0);
        config.capabilities = CAP_BLOCKS | if args.serve_witness { CAP_WITNESS } else { 0 };
        config.bootnodes = args.bootnodes;
        config.trusted = args.trusted;
        config.advertise = args.advertise;
        config.requests = kinds();
        config.notices = vec![NotifyKind { kind: ANNOUNCE, max_len: 40, per_peer_per_second: 20 }];
        config.max_gossip_len = 0;
        let (handle, events) =
            base_tiger_p2p::start(config, Some(args.listen), false).map_err(|error| format!("starting tp2p: {error}"))?;
        let public = PublicKey::from_secret_key(secp256k1::SECP256K1, &args.secret);
        let addr = args.advertise.or(handle.local_addr()).expect("listening");
        eprintln!("tiger: tp2p://{}@{addr}", alloy_primitives::hex::encode(public.serialize()));
        let tp2p = Arc::new(Tp2p { handle, waiting: Mutex::new(Waiting::default()) });
        assert!(self.tp2p.set(Arc::clone(&tp2p)).is_ok(), "tp2p starts once");

        // Requests go to a bounded pool; a full pool answers Busy.
        let (jobs, job_receiver) = mpsc::sync_channel::<(PeerId, u32, u16, Vec<u8>)>(256);
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        for _ in 0..SERVE_WORKERS {
            let (node, tp2p, receiver) = (Arc::clone(self), Arc::clone(&tp2p), Arc::clone(&job_receiver));
            thread::Builder::new()
                .name("tp2p-serve".into())
                .spawn(move || {
                    loop {
                        let job = receiver.lock().expect("tp2p jobs").recv();
                        let Ok((peer, id, kind, body)) = job else { return };
                        let (code, answer) = node.serve(kind, &body);
                        tp2p.handle.respond(peer, id, kind, code, answer);
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        {
            let (node, tp2p) = (Arc::clone(self), Arc::clone(&tp2p));
            thread::Builder::new()
                .name("tp2p-events".into())
                .spawn(move || {
                    while let Ok(event) = events.recv() {
                        match event {
                            Event::Request { peer, id, kind, body } => {
                                if jobs.try_send((peer, id, kind, body)).is_err() {
                                    tp2p.handle.respond(peer, id, kind, ResponseCode::Busy, Vec::new());
                                }
                            }
                            Event::Response { request, peer, result } => tp2p.answered(request, peer, result),
                            Event::Notify { kind: ANNOUNCE, body, .. } if body.len() == 40 => {
                                let hash = B256::from_slice(&body[..32]);
                                let number = u64::from_be_bytes(body[32..].try_into().expect("8 bytes"));
                                // A stateless node prefetches the witness it is about to need.
                                if let Some(witnesses) = &node.witnesses
                                    && node.chain.read().expect("chain lock").block(&hash).is_none()
                                {
                                    witnesses.request(hash, number);
                                }
                            }
                            Event::PeerReady { peer, status } => {
                                eprintln!("tiger: tp2p peer {} ready (capabilities {:#x})", alloy_primitives::hex::encode(peer), status.capabilities);
                            }
                            Event::PeerGone { peer } => eprintln!("tiger: tp2p peer {} gone", alloy_primitives::hex::encode(peer)),
                            Event::Gossip { .. } | Event::Notify { .. } => {}
                        }
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        if args.serve_witness
            && let Some(cache) = &self.witness_cache
        {
            let built = cache.subscribe();
            let tp2p = Arc::clone(&tp2p);
            thread::Builder::new()
                .name("tp2p-announce".into())
                .spawn(move || {
                    while let Ok((hash, number)) = built.recv() {
                        let mut body = hash.to_vec();
                        body.extend_from_slice(&number.to_be_bytes());
                        tp2p.handle.notify(None, ANNOUNCE, body);
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(tp2p)
    }

    /// Answers one tp2p request.
    fn serve(&self, kind: u16, body: &[u8]) -> (ResponseCode, Vec<u8>) {
        match kind {
            GET_HEADERS => {
                let Ok(request) = decode_headers_request(body) else { return (ResponseCode::BadRequest, Vec::new()) };
                let headers = self.serve_headers(&request);
                if headers.is_empty() {
                    return (ResponseCode::NotFound, Vec::new());
                }
                let payload: usize = headers.iter().map(Vec::len).sum();
                let mut out = Vec::with_capacity(payload + 9);
                alloy_rlp::Header { list: true, payload_length: payload }.encode(&mut out);
                for header in headers {
                    out.extend_from_slice(&header);
                }
                (ResponseCode::Ok, base_tiger_net::snappy_compress(&out))
            }
            GET_BODIES => {
                let Ok(hashes) = Vec::<B256>::decode(&mut &body[..]) else { return (ResponseCode::BadRequest, Vec::new()) };
                // Stop near 3/4 of the answer limit (uncompressed), as devp2p's soft limit does.
                let (mut bodies, mut bytes) = (Vec::new(), 0);
                for hash in hashes.iter().take(MAX_BODIES) {
                    let Some(body) = self.body_by_hash(*hash) else { break };
                    bytes += alloy_rlp::Encodable::length(&body);
                    if !bodies.is_empty() && bytes > MAX_BODIES_BYTES / 4 * 3 {
                        break;
                    }
                    bodies.push(body);
                }
                if bodies.is_empty() {
                    return (ResponseCode::NotFound, Vec::new());
                }
                (ResponseCode::Ok, base_tiger_net::snappy_compress(&alloy_rlp::encode(&bodies)))
            }
            GET_WITNESS if body.len() == 40 => {
                let hash = B256::from_slice(&body[..32]);
                match self.witness_cache.as_ref().and_then(|cache| cache.get(&hash, WITNESS_WAIT)) {
                    Some(witness) => (ResponseCode::Ok, witness.as_ref().clone()),
                    None => (ResponseCode::NotFound, Vec::new()),
                }
            }
            _ => (ResponseCode::BadRequest, Vec::new()),
        }
    }

    /// Headers for a request, at most [`MAX_HEADERS`] and about 4 MiB. A reverse walk without
    /// skips follows parent hashes, so it serves side branches too.
    fn serve_headers(&self, request: &HeadersRequest) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let first = match request.start {
            BlockId::Hash(hash) => self.header_by_hash(hash),
            BlockId::Number(number) => self.header_by_number(number),
        };
        let limit = request.limit.min(MAX_HEADERS);
        let Some(mut current) = first.filter(|_| limit > 0) else { return out };
        let step = request.skip.saturating_add(1);
        let mut bytes = 0;
        loop {
            let encoded = alloy_rlp::encode(&current);
            bytes += encoded.len();
            let parent = current.parent_hash;
            let number = current.number;
            out.push(encoded);
            if out.len() as u64 >= limit || bytes > (3 << 20) {
                return out;
            }
            let next: Option<Header> = if request.reverse && request.skip == 0 {
                self.header_by_hash(parent)
            } else if request.reverse {
                number.checked_sub(step).and_then(|n| self.header_by_number(n))
            } else {
                number.checked_add(step).and_then(|n| self.header_by_number(n))
            };
            match next {
                Some(header) => current = header,
                None => return out,
            }
        }
    }
}

fn decode_headers_request(mut body: &[u8]) -> alloy_rlp::Result<HeadersRequest> {
    // `[request_id, [start, limit, skip, reverse]]`, as `HeadersRequest::encode_payload` writes.
    let outer = alloy_rlp::Header::decode(&mut body)?;
    if !outer.list || outer.payload_length != body.len() {
        return Err(alloy_rlp::Error::UnexpectedString);
    }
    let _request_id = u64::decode(&mut body)?;
    let request = HeadersRequest::decode(&mut body)?;
    if !body.is_empty() {
        return Err(alloy_rlp::Error::UnexpectedLength);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use alloy_consensus::Header;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use secp256k1::{PublicKey, SecretKey};
    use serde_json::json;

    use base_tiger_evm::BlockRules;
    use base_tiger_net::{
        BlockId, DownloadConfig, Downloader, ForkSchedule, HeadersRequest, NetworkConfig, NodeRecord,
    };

    use super::Tp2pArgs;
    use crate::{BlockExecution, Fetcher, Node, WitnessCache, rpc::test_utils::node_with};

    const CLEARER: &str = "0x00000000000000000000000000000000000000c0";

    fn genesis() -> serde_json::Value {
        json!({ "extraData": "0x0100000032000000060000000000000000", "alloc": {
            CLEARER: { "balance": "0x0", "code": "0x6000600155600060025500", "storage": {
                "0x0000000000000000000000000000000000000000000000000000000000000001": "0x01",
                "0x0000000000000000000000000000000000000000000000000000000000000002": "0x02",
                "0x0000000000000000000000000000000000000000000000000000000000000003": "0x03"
            } }
        } })
    }

    fn deposit(to: Address, nonce: u64) -> Bytes {
        deposit_with(to, nonce, Bytes::new())
    }

    fn deposit_with(to: Address, nonce: u64, input: Bytes) -> Bytes {
        let tx = TxDeposit {
            source_hash: B256::from(U256::from(nonce)),
            from: Address::with_last_byte(0x10),
            to: TxKind::Call(to),
            mint: 1,
            value: U256::from(1),
            gas_limit: 200_000,
            is_system_transaction: false,
            input,
        };
        BaseTxEnvelope::Deposit(alloy_consensus::Sealed::new(tx)).encoded_2718().into()
    }

    /// Builds the next block on `node` and imports it through validation (recording reads for
    /// the witness cache), making it canonical.
    fn advance(node: &Node, transactions: &[Bytes]) -> (Header, B256) {
        let chain = node.chain.read().unwrap();
        let parent = chain.head();
        let template = Header {
            parent_hash: parent.hash,
            number: parent.number() + 1,
            timestamp: parent.header.timestamp + 2,
            gas_limit: parent.header.gas_limit,
            base_fee_per_gas: BlockRules { chain: &node.spec }.next_base_fee(&parent.header, parent.header.timestamp + 2),
            beneficiary: parent.header.beneficiary,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Header::default()
        };
        let mut extra = vec![1, 0, 0, 0, 50, 0, 0, 0, 6];
        extra.extend_from_slice(&[0; 8]);
        let state = chain.state_after(&parent).unwrap();
        let built = BlockExecution { chain: &node.spec }
            .build(&BlockExecution::env(&template), extra.into(), Some(0), transactions, false, &state)
            .unwrap();
        drop(state);
        drop(chain);
        let header = built.executed.expect("the block builds").header;
        let hash = header.hash_slow();
        let validated = node.validate_block(&header, hash, transactions).unwrap().unwrap();
        let mut engine = node.engine.lock().unwrap();
        let imported = node.store_validated(&mut engine, header.clone(), hash, validated).unwrap();
        assert!(matches!(imported, crate::Imported::Valid), "{imported:?}");
        drop(engine);
        node.chain.write().unwrap().set_canonical(hash).unwrap();
        (header, hash)
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(180);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A full node syncs 40 blocks from another over tp2p alone, and fetches a block's witness
    /// that the serving node built from recorded reads; the witness verifies on the stateless path.
    #[test]
    fn sync_and_witness_over_tp2p() {
        let mut a = node_with(0, genesis());
        a.witness_cache = Some(WitnessCache::new(64));
        let a = Arc::new(a);
        a.start_witness_builder().unwrap();
        let a_key = SecretKey::from_byte_array([0x41; 32]).unwrap();
        let a_tp2p = a
            .start_tp2p(Tp2pArgs {
                listen: "127.0.0.1:0".parse().unwrap(),
                advertise: None,
                bootnodes: Vec::new(),
                trusted: Vec::new(),
                serve_witness: true,
                secret: a_key,
            })
            .unwrap();
        let mut blocks = Vec::new();
        for i in 0..40u64 {
            let mut txs = vec![deposit(Address::with_last_byte(0x20 + (i % 5) as u8), i)];
            if i == 39 {
                txs.push(deposit(CLEARER.parse().unwrap(), 1000));
            }
            blocks.push((advance(&a, &txs), txs));
        }
        let ((last_header, last_hash), last_txs) = blocks.last().cloned().unwrap();

        let b = Arc::new(node_with(0, genesis()));
        let b_tp2p = b
            .start_tp2p(Tp2pArgs {
                listen: "127.0.0.1:0".parse().unwrap(),
                advertise: None,
                bootnodes: vec![(PublicKey::from_secret_key(secp256k1::SECP256K1, &a_key), a_tp2p.handle().local_addr().unwrap())],
                trusted: Vec::new(),
                serve_witness: false,
                secret: SecretKey::from_byte_array([0x42; 32]).unwrap(),
            })
            .unwrap();
        b.start_sync_with(None, Fetcher::Tp2p(Arc::clone(&b_tp2p))).unwrap();
        until("a tp2p peer", || !b_tp2p.handle().peers().is_empty());
        b.request_sync(last_header.parent_hash);
        until("the sync", || b.chain.read().unwrap().block(&last_header.parent_hash).is_some());

        let witness = b_tp2p.fetch_witness(last_hash, last_header.number).unwrap();
        let chain = b.chain.read().unwrap();
        let parent = chain.block(&last_header.parent_hash).unwrap();
        let (validated, _) = b
            .validate_with_witness(&chain, witness, &last_header, last_hash, &last_txs, &parent)
            .expect("the witness from the peer verifies");
        assert!(validated.invalid.is_none());
        assert!(a_tp2p.handle().traffic().bytes_out.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    /// Counts the bytes a loopback TCP proxy to `target` carries, both ways.
    fn counting_proxy(target: SocketAddr) -> (SocketAddr, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bytes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&bytes);
        std::thread::spawn(move || {
            for inbound in listener.incoming().flatten() {
                let Ok(outbound) = TcpStream::connect(target) else { continue };
                for (mut from, mut to) in [
                    (inbound.try_clone().unwrap(), outbound.try_clone().unwrap()),
                    (outbound, inbound),
                ] {
                    let counter = Arc::clone(&counter);
                    std::thread::spawn(move || {
                        let mut buffer = vec![0; 64 << 10];
                        while let Ok(read) = from.read(&mut buffer) {
                            if read == 0 || to.write_all(&buffer[..read]).is_err() {
                                break;
                            }
                            counter.fetch_add(read as u64, Ordering::Relaxed);
                        }
                        let _ = to.shutdown(std::net::Shutdown::Both);
                    });
                }
            }
        });
        (addr, bytes)
    }

    /// The process's user plus system CPU time, from `/proc/self/stat` (clock ticks of 10 ms).
    fn cpu() -> Duration {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
        let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        Duration::from_millis(ticks * 10)
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn devp2p_config(node: &Node, key: secp256k1::SecretKey, port: u16) -> NetworkConfig {
        let genesis = node.chain.read().unwrap().genesis.clone();
        let forks = ForkSchedule::base(genesis.hash, genesis.header.timestamp, 0, &[]);
        let mut config = NetworkConfig::new(key, node.spec.chain_id, forks);
        config.listen_addr = Some(SocketAddr::from(([127, 0, 0, 1], port)));
        config.discovery_addr = None;
        config
    }

    fn percentile(samples: &mut [Duration], p: usize) -> Duration {
        samples.sort();
        samples[(samples.len() * p / 100).min(samples.len() - 1)]
    }

    /// Measures EL sync between two tiger nodes over devp2p and over tp2p on the same chain:
    /// bytes on the wire (both directions, handshakes included), wall time, process CPU, single
    /// block (header + body) fetch latency, and witness fetch latency. Run with
    /// `TP2P_BENCH_BLOCKS=500 cargo test -p base-tiger --release --features tp2p --lib
    /// measure_sync -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, minutes in debug builds"]
    fn measure_sync_devp2p_vs_tp2p() {
        let blocks: u64 = std::env::var("TP2P_BENCH_BLOCKS").ok().and_then(|n| n.parse().ok()).unwrap_or(200);
        let txs_per_block: u64 = std::env::var("TP2P_BENCH_TXS").ok().and_then(|n| n.parse().ok()).unwrap_or(40);
        let mut a = node_with(0, genesis());
        a.witness_cache = Some(WitnessCache::new(64));
        let a = Arc::new(a);
        a.start_witness_builder().unwrap();
        let started = Instant::now();
        let mut head = (Header::default(), B256::ZERO);
        for i in 0..blocks {
            let txs: Vec<Bytes> = (0..txs_per_block)
                .map(|t| {
                    let n = i * txs_per_block + t;
                    {
                        // Incompressible input, like real calldata after snappy (devp2p compresses).
                        let input: Vec<u8> = (0..8u64).flat_map(|k| alloy_primitives::keccak256((n * 8 + k).to_be_bytes()).0).collect();
                        deposit_with(Address::from_word(B256::from(U256::from(n % 997 + 0x100))), n, input.into())
                    }
                })
                .collect();
            head = advance(&a, &txs);
        }
        let (head_header, head_hash) = head;
        eprintln!("bench: built {blocks} blocks of {txs_per_block} txs in {:?}", started.elapsed());

        // Server A: devp2p and tp2p, each behind a counting proxy.
        let a_key = secp256k1::SecretKey::from_byte_array([0x41; 32]).unwrap();
        let a_port = free_port();
        a.start_sync(devp2p_config(&a, a_key, a_port)).unwrap();
        let a_tp2p = a
            .start_tp2p(Tp2pArgs {
                listen: "127.0.0.1:0".parse().unwrap(),
                advertise: None,
                bootnodes: Vec::new(),
                trusted: Vec::new(),
                serve_witness: true,
                secret: a_key,
            })
            .unwrap();
        let (devp2p_proxy, devp2p_bytes) = counting_proxy(SocketAddr::from(([127, 0, 0, 1], a_port)));
        let (tp2p_proxy, tp2p_bytes) = counting_proxy(a_tp2p.handle().local_addr().unwrap());

        // B syncs over devp2p.
        let b = Arc::new(node_with(0, genesis()));
        let mut config = devp2p_config(&b, secp256k1::SecretKey::from_byte_array([0x42; 32]).unwrap(), free_port());
        let a_id = a.sync.get().unwrap().network.as_ref().unwrap().local_id();
        config.trusted_peers = vec![NodeRecord { id: a_id, addr: devp2p_proxy, udp_port: 0 }];
        b.start_sync(config).unwrap();
        let b_network = b.sync.get().unwrap().network.clone().unwrap();
        until("a devp2p peer", || b_network.peer_count() > 0);
        let (bytes0, cpu0, wall0) = (devp2p_bytes.load(Ordering::Relaxed), cpu(), Instant::now());
        b.request_sync(head_hash);
        until("the devp2p sync", || b.chain.read().unwrap().block(&head_hash).is_some());
        let devp2p = (devp2p_bytes.load(Ordering::Relaxed) - bytes0, wall0.elapsed(), cpu() - cpu0);

        // C syncs over tp2p.
        let c = Arc::new(node_with(0, genesis()));
        let c_tp2p = c
            .start_tp2p(Tp2pArgs {
                listen: "127.0.0.1:0".parse().unwrap(),
                advertise: None,
                bootnodes: vec![(PublicKey::from_secret_key(secp256k1::SECP256K1, &a_key), tp2p_proxy)],
                trusted: Vec::new(),
                serve_witness: false,
                secret: secp256k1::SecretKey::from_byte_array([0x43; 32]).unwrap(),
            })
            .unwrap();
        c.start_sync_with(None, Fetcher::Tp2p(Arc::clone(&c_tp2p))).unwrap();
        until("a tp2p peer", || !c_tp2p.handle().peers().is_empty());
        let (bytes0, cpu0, wall0) = (tp2p_bytes.load(Ordering::Relaxed), cpu(), Instant::now());
        c.request_sync(head_hash);
        until("the tp2p sync", || c.chain.read().unwrap().block(&head_hash).is_some());
        let tp2p = (tp2p_bytes.load(Ordering::Relaxed) - bytes0, wall0.elapsed(), cpu() - cpu0);

        let mib = |bytes: u64| bytes as f64 / (1 << 20) as f64;
        // Download only (no import): every header and body, as the transports alone cost.
        let downloader = Downloader::new(b_network.clone(), DownloadConfig::default());
        let (bytes0, cpu0, wall0) = (devp2p_bytes.load(Ordering::Relaxed), cpu(), Instant::now());
        for start in (1..=head_header.number).step_by(512) {
            let request = HeadersRequest { start: BlockId::Number(start), limit: 512, skip: 0, reverse: false };
            let headers = downloader.headers(request).unwrap();
            downloader.bodies(&headers, &mut |_| {}).unwrap();
        }
        let devp2p_download = (devp2p_bytes.load(Ordering::Relaxed) - bytes0, wall0.elapsed(), cpu() - cpu0);
        let (bytes0, cpu0, wall0) = (tp2p_bytes.load(Ordering::Relaxed), cpu(), Instant::now());
        for start in (1..=head_header.number).step_by(512) {
            let request = HeadersRequest { start: BlockId::Number(start), limit: 512, skip: 0, reverse: false };
            let headers = c_tp2p.headers(request).unwrap();
            c_tp2p.bodies(&headers).unwrap();
        }
        let tp2p_download = (tp2p_bytes.load(Ordering::Relaxed) - bytes0, wall0.elapsed(), cpu() - cpu0);
        eprintln!("bench: download only, devp2p  {:8.2} MiB  wall {:>10.2?}  cpu {:>10.2?}", mib(devp2p_download.0), devp2p_download.1, devp2p_download.2);
        eprintln!("bench: download only, tp2p    {:8.2} MiB  wall {:>10.2?}  cpu {:>10.2?}", mib(tp2p_download.0), tp2p_download.1, tp2p_download.2);
        eprintln!("bench: sync of {blocks} blocks ({txs_per_block} txs of 256 B input each)");
        eprintln!("bench: devp2p  {:8.2} MiB  wall {:>10.2?}  cpu {:>10.2?}", mib(devp2p.0), devp2p.1, devp2p.2);
        eprintln!("bench: tp2p    {:8.2} MiB  wall {:>10.2?}  cpu {:>10.2?}", mib(tp2p.0), tp2p.1, tp2p.2);

        // One block (header by number, then its body), 200 times, over each, paced under
        // tp2p's per-peer request rate limit (50 a second).
        let (mut devp2p_fetch, mut tp2p_fetch) = (Vec::new(), Vec::new());
        for i in 0..200u64 {
            let number = 1 + (i * 7919) % head_header.number;
            let request = HeadersRequest { start: BlockId::Number(number), limit: 1, skip: 0, reverse: false };
            let t = Instant::now();
            let headers = downloader.headers(request).unwrap();
            downloader.bodies(&headers, &mut |_| {}).unwrap();
            devp2p_fetch.push(t.elapsed());
            let t = Instant::now();
            let headers = c_tp2p.headers(request).unwrap();
            c_tp2p.bodies(&headers).unwrap();
            tp2p_fetch.push(t.elapsed());
            std::thread::sleep(Duration::from_millis(25));
        }

        // Witnesses of the last blocks the cache holds.
        let mut witness = Vec::new();
        let mut witness_bytes = 0;
        let chain = a.chain.read().unwrap();
        let recent: Vec<(B256, u64)> = (head_header.number.saturating_sub(31)..=head_header.number)
            .filter_map(|n| chain.canonical_hash(n).map(|hash| (hash, n)))
            .collect();
        drop(chain);
        for (hash, number) in recent {
            let t = Instant::now();
            let fetched = c_tp2p.fetch_witness(hash, number).unwrap();
            witness.push(t.elapsed());
            witness_bytes += crate::encode_witness(&fetched).len();
            std::thread::sleep(Duration::from_millis(60));
        }
        let witnesses = witness.len();

        eprintln!(
            "bench: one block  devp2p p50 {:?} p99 {:?}  tp2p p50 {:?} p99 {:?}",
            percentile(&mut devp2p_fetch, 50),
            percentile(&mut devp2p_fetch, 99),
            percentile(&mut tp2p_fetch, 50),
            percentile(&mut tp2p_fetch, 99)
        );
        eprintln!(
            "bench: witness over tp2p  {witnesses} blocks, mean {} B, p50 {:?} p99 {:?}",
            witness_bytes / witnesses.max(1),
            percentile(&mut witness, 50),
            percentile(&mut witness, 99)
        );
    }
}
