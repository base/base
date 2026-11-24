//! The upstream base-reth JSON-RPC endpoint a stateless node reads what it does not hold: the
//! start block, and an execution witness for each block whose state is not all local.
//!
//! The client speaks HTTP/1.1 (one request per connection), plain or over TLS (rustls, Mozilla
//! roots). Every answer is verified (witness and proof nodes by hash against tiger's own state
//! roots, blocks by hash), so the channel adds no trust; TLS keeps an endpoint's key private.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes, U256};
use serde::Deserialize;
use serde_json::{Value, json};

/// How long one upstream request may take, connect to last byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Witness requests in flight at once: generating one takes the upstream seconds for a full
/// mainnet block, and it serves many in parallel.
const WITNESS_WORKERS: usize = 16;
/// Fetched witnesses kept until taken; older ones (of blocks that never came) are dropped.
const MAX_READY_WITNESSES: usize = 512;

/// The largest upstream response read (a witness of a full mainnet block is a few megabytes).
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// An execution witness as base-reth's `debug_executionWitness` returns it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExecutionWitness {
    /// Trie nodes (RLP) the block reads and its root computation needs.
    pub state: Vec<Bytes>,
    /// Bytecodes the block runs.
    pub codes: Vec<Bytes>,
    /// Ancestor headers (RLP) back to the oldest block hash the block reads.
    #[serde(default)]
    pub headers: Vec<Bytes>,
}

/// An upstream JSON-RPC endpoint (`http://` or `https://host:port/path`).
#[derive(Debug, Clone)]
pub struct Upstream {
    host: String,
    port: u16,
    path: String,
    /// TLS settings for an `https://` endpoint, which the Mozilla root certificates verify.
    tls: Option<Arc<rustls::ClientConfig>>,
}

/// A connection to the upstream, plain or TLS.
trait Connection: Read + Write {}
impl<T: Read + Write> Connection for T {}

impl Upstream {
    /// Parses `url`, which must be `http://` or `https://host[:port][/path]`.
    pub fn new(url: &str) -> Result<Self, String> {
        let (rest, tls) = match (url.strip_prefix("http://"), url.strip_prefix("https://")) {
            (Some(rest), _) => (rest, false),
            (None, Some(rest)) => (rest, true),
            (None, None) => {
                return Err(format!("upstream {url}: only http:// and https:// are supported"));
            }
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>().map_err(|error| format!("upstream {url}: port: {error}"))?,
            ),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(format!("upstream {url}: no host"));
        }
        let tls = tls.then(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map(|builder| {
                    Arc::new(builder.with_root_certificates(roots).with_no_client_auth())
                })
                .map_err(|error| format!("upstream {url}: tls: {error}"))
        });
        Ok(Self { host: host.to_owned(), port, path: path.to_owned(), tls: tls.transpose()? })
    }

    /// Calls `method` with `params` and returns its result.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
        let response = self.post(&body).map_err(|error| format!("upstream {method}: {error}"))?;
        let mut reply: Value =
            serde_json::from_slice(&response).map_err(|error| format!("upstream {method}: reply: {error}"))?;
        if let Some(error) = reply.get("error") {
            return Err(format!("upstream {method}: {error}"));
        }
        Ok(reply["result"].take())
    }

    /// Calls every `(method, params)` in one JSON-RPC batch; results in the same order.
    pub fn call_batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>, String> {
        let batch: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(id, (method, params))| json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .collect();
        let response = self
            .post(&Value::Array(batch).to_string())
            .map_err(|error| format!("upstream batch: {error}"))?;
        let replies: Vec<Value> = serde_json::from_slice(&response)
            .map_err(|error| format!("upstream batch: reply: {error}"))?;
        let mut results = vec![Value::Null; calls.len()];
        let mut answered = 0;
        for mut reply in replies {
            let id = reply["id"]
                .as_u64()
                .and_then(|id| usize::try_from(id).ok())
                .filter(|id| *id < calls.len());
            let Some(id) = id else {
                return Err("upstream batch: reply with an unknown id".into());
            };
            if let Some(error) = reply.get("error") {
                return Err(format!("upstream {}: {error}", calls[id].0));
            }
            results[id] = reply["result"].take();
            answered += 1;
        }
        if answered != calls.len() {
            return Err("upstream batch: missing replies".into());
        }
        Ok(results)
    }

    /// The trie nodes proving `keys` (accounts with storage slots) and the code of `codes`
    /// (accounts), at block `block`, from `eth_getProof` and `eth_getCode` in one batch. Nothing
    /// in them is trusted: the caller keeps only the nodes that link by hash to the block's state
    /// root and the code that hashes to a known code hash.
    pub fn proofs(
        &self,
        block: B256,
        keys: &[(Address, Vec<U256>)],
        codes: &[Address],
    ) -> Result<(Vec<Bytes>, Vec<Bytes>), String> {
        let mut calls = Vec::with_capacity(keys.len() + codes.len());
        for (address, slots) in keys {
            let slots: Vec<B256> = slots.iter().map(|slot| B256::from(*slot)).collect();
            calls.push(("eth_getProof", json!([address, slots, block])));
        }
        for address in codes {
            calls.push(("eth_getCode", json!([address, block])));
        }
        let results = self.call_batch(&calls)?;
        let mut nodes = Vec::new();
        let bytes = |value: &Value| {
            serde_json::from_value::<Bytes>(value.clone()).map_err(|error| error.to_string())
        };
        for result in &results[..keys.len()] {
            for node in result["accountProof"].as_array().into_iter().flatten() {
                nodes.push(bytes(node)?);
            }
            for storage in result["storageProof"].as_array().into_iter().flatten() {
                for node in storage["proof"].as_array().into_iter().flatten() {
                    nodes.push(bytes(node)?);
                }
            }
        }
        let codes = results[keys.len()..].iter().map(bytes).collect::<Result<Vec<_>, _>>()?;
        Ok((nodes, codes))
    }

    /// The execution witness of block `hash` (number `number`). By number it is the witness of
    /// the upstream's canonical block at `number`, which may be another block (it then fails
    /// verification against tiger's roots); `by_hash` asks for `hash` itself, which base-reth
    /// serves several times slower (no proofs-history index), falling back to the number when the
    /// upstream lacks the method.
    pub fn execution_witness(
        &self,
        hash: B256,
        number: u64,
        by_hash: bool,
    ) -> Result<ExecutionWitness, String> {
        let by_number = || self.call("debug_executionWitness", json!([format!("{number:#x}")]));
        let result =
            match by_hash.then(|| self.call("debug_executionWitnessByBlockHash", json!([hash]))) {
                None => by_number()?,
                Some(Ok(result)) => result,
                // Only an upstream without the method falls back to the block number: one that
                // lacks the block (a reorged-out branch) would answer for another block.
                Some(Err(error)) if error.contains("-32601") => by_number()?,
                Some(Err(error)) => return Err(error),
            };
        serde_json::from_value(result)
            .map_err(|error| format!("upstream debug_executionWitness: {error}"))
    }

    /// The keys canonical block `number` reads and deletes, from the upstream's
    /// `prestateTracer`. Only a hint of which keys to prove: a reorged block's keys differ, and
    /// every value comes from proofs instead.
    pub fn block_keys(&self, number: u64) -> Result<BlockKeys, String> {
        let block = format!("{number:#x}");
        let reads = json!({ "tracer": "prestateTracer", "tracerConfig": { "disableCode": true } });
        let diff = json!({ "tracer": "prestateTracer", "tracerConfig": { "diffMode": true, "disableCode": true } });
        let traces = self.call_batch(&[
            ("debug_traceBlockByNumber", json!([block, reads])),
            ("debug_traceBlockByNumber", json!([block, diff])),
        ])?;
        let parse = |address: &String| {
            address.parse::<Address>().map_err(|error| format!("prestate account: {error}"))
        };
        let slots = |account: &Value| {
            account["storage"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(slot, _)| {
                    slot.parse::<U256>().map_err(|error| format!("prestate slot: {error}"))
                })
                .collect::<Result<BTreeSet<U256>, String>>()
        };
        let mut keys = BlockKeys::default();
        for trace in traces[0].as_array().into_iter().flatten() {
            for (address, account) in trace["result"].as_object().into_iter().flatten() {
                keys.read.entry(parse(address)?).or_default().extend(slots(account)?);
            }
        }
        // In diff mode a slot set to zero is in `pre` but not `post`, and so is a deleted account.
        for trace in traces[1].as_array().into_iter().flatten() {
            let post = &trace["result"]["post"];
            for (address, account) in trace["result"]["pre"].as_object().into_iter().flatten() {
                let after = &post[address.as_str()];
                let kept = slots(after)?;
                let deleted: BTreeSet<U256> = slots(account)?.difference(&kept).copied().collect();
                if after.is_null() || !deleted.is_empty() {
                    keys.deleted.entry(parse(address)?).or_default().extend(deleted);
                }
            }
        }
        Ok(keys)
    }

    /// The RLP of canonical block `block` (a number or tag) and its receipts (EIP-2718).
    pub fn raw_block(&self, block: &str) -> Result<(Bytes, Vec<Bytes>), String> {
        let block_rlp: Bytes = serde_json::from_value(self.call("debug_getRawBlock", json!([block]))?)
            .map_err(|error| format!("upstream debug_getRawBlock: {error}"))?;
        let receipts: Vec<Bytes> = serde_json::from_value(self.call("debug_getRawReceipts", json!([block]))?)
            .map_err(|error| format!("upstream debug_getRawReceipts: {error}"))?;
        Ok((block_rlp, receipts))
    }

    fn post(&self, body: &str) -> std::io::Result<Vec<u8>> {
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("host does not resolve"))?;
        let tcp = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT)?;
        tcp.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        tcp.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut stream: Box<dyn Connection> = match &self.tls {
            None => Box::new(tcp),
            Some(config) => {
                let name = rustls::pki_types::ServerName::try_from(self.host.clone())
                    .map_err(std::io::Error::other)?;
                let connection = rustls::ClientConnection::new(Arc::clone(config), name)
                    .map_err(std::io::Error::other)?;
                Box::new(rustls::StreamOwned::new(connection, tcp))
            }
        };
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: base-tiger\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.path,
            self.host,
            self.port,
            body.len()
        )?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
        if status != "200" {
            return Err(std::io::Error::other(format!("HTTP status {}", line.trim())));
        }
        let (mut length, mut chunked) = (None, false);
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.parse::<usize>().map_err(std::io::Error::other)?);
            } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
                    .map_err(std::io::Error::other)?;
                if size == 0 {
                    break;
                }
                if body.len() + size > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::other("response too large"));
                }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..])?;
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf)?;
            }
        } else if let Some(length) = length {
            if length > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::other("response too large"));
            }
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else {
            reader.take(MAX_RESPONSE_BYTES as u64).read_to_end(&mut body)?;
        }
        Ok(body)
    }
}

/// The accounts and storage slots a block reads, and the accounts it deletes (with no slots) and
/// the slots it sets to zero.
#[derive(Debug, Default)]
pub struct BlockKeys {
    /// Read keys.
    pub read: BTreeMap<Address, BTreeSet<U256>>,
    /// Deleted keys.
    pub deleted: BTreeMap<Address, BTreeSet<U256>>,
}

/// Execution witnesses fetched ahead from the upstream, many at a time, so a stateless node keeps
/// up although each witness takes the upstream seconds to generate.
#[derive(Debug)]
pub struct Witnesses {
    jobs: Mutex<mpsc::Sender<(B256, u64, bool)>>,
    shared: Arc<(Mutex<WitnessSlots>, Condvar)>,
}

/// Fetched and in-flight witnesses, by block hash.
#[derive(Debug, Default)]
struct WitnessSlots {
    ready: HashMap<B256, Result<ExecutionWitness, String>>,
    order: VecDeque<B256>,
    inflight: HashSet<B256>,
    /// Blocks whose witness by number did not verify: fetched by hash instead.
    by_hash: HashSet<B256>,
}

impl Witnesses {
    /// Starts the fetch workers for `upstream`.
    pub fn new(upstream: &Upstream) -> Self {
        let (sender, receiver) = mpsc::channel::<(B256, u64, bool)>();
        let receiver = Arc::new(Mutex::new(receiver));
        let shared = Arc::new((Mutex::new(WitnessSlots::default()), Condvar::new()));
        for _ in 0..WITNESS_WORKERS {
            let (upstream, receiver, shared) = (upstream.clone(), Arc::clone(&receiver), Arc::clone(&shared));
            thread::spawn(move || {
                loop {
                    let job = receiver.lock().expect("witness jobs").recv();
                    let Ok((hash, number, by_hash)) = job else { return };
                    let witness = upstream.execution_witness(hash, number, by_hash);
                    let (slots, ready) = &*shared;
                    let mut slots = slots.lock().expect("witness slots");
                    slots.inflight.remove(&hash);
                    slots.ready.insert(hash, witness);
                    slots.order.push_back(hash);
                    while slots.order.len() > MAX_READY_WITNESSES {
                        let oldest = slots.order.pop_front().expect("over capacity");
                        slots.ready.remove(&oldest);
                    }
                    ready.notify_all();
                }
            });
        }
        Self { jobs: Mutex::new(sender), shared }
    }

    /// Starts fetching the witness of block `hash` (number `number`) unless it is fetched or on
    /// its way.
    pub fn request(&self, hash: B256, number: u64) {
        let mut slots = self.shared.0.lock().expect("witness slots");
        if slots.ready.contains_key(&hash) || !slots.inflight.insert(hash) {
            return;
        }
        let by_hash = slots.by_hash.contains(&hash);
        drop(slots);
        let _ = self.jobs.lock().expect("witness jobs").send((hash, number, by_hash));
    }

    /// Fetches the witness of block `hash` again, by hash, after the one by number did not
    /// verify. Returns false when it was already fetched by hash.
    pub fn retry_by_hash(&self, hash: B256, number: u64) -> bool {
        if !self.shared.0.lock().expect("witness slots").by_hash.insert(hash) {
            return false;
        }
        self.request(hash, number);
        true
    }

    /// The fetched witness of block `hash`, removed, if it has arrived.
    pub fn take(&self, hash: &B256) -> Option<Result<ExecutionWitness, String>> {
        let mut slots = self.shared.0.lock().expect("witness slots");
        let witness = slots.ready.remove(hash)?;
        slots.order.retain(|other| other != hash);
        Some(witness)
    }

    /// Forgets that block `hash` is fetched by hash (once its block is imported or rejected).
    pub fn forget(&self, hash: &B256) {
        self.shared.0.lock().expect("witness slots").by_hash.remove(hash);
    }

    /// Requests the witness of block `hash` and waits until it has arrived (it stays for
    /// [`Self::take`]), at most `timeout`. Returns whether it arrived.
    pub fn wait(&self, hash: B256, number: u64, timeout: Duration) -> bool {
        self.request(hash, number);
        let (slots, ready) = &*self.shared;
        let slots = slots.lock().expect("witness slots");
        let (slots, _) = ready
            .wait_timeout_while(slots, timeout, |slots| !slots.ready.contains_key(&hash))
            .expect("witness slots");
        slots.ready.contains_key(&hash)
    }
}
