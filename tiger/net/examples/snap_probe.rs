//! Snap-syncs a peer's state into memory and checks the result's state root. Used for interop
//! tests against base-reth started with `--snap.v2`.
//!
//! Without `--rpc`, the pivot is the peer's head and the chain must not move. With `--rpc`, the
//! probe follows the chain while it downloads, like a stateless tiger: for every new block it
//! reads the block's state changes from the node (`debug_traceBlockByNumber` with the prestate
//! tracer in diff mode, plus the EIP-4788 and EIP-2935 system contract slots that run outside
//! transactions), applies them to its flat state, and reports the touched keys, so snap's
//! healing is exercised against a moving pivot.
//!
//! ```text
//! cargo run --release --example snap_probe -- --chain-id 84538453 --genesis <hash> \
//!     --genesis-time <t> --peer enode://... [--rpc http://127.0.0.1:8545]
//! ```

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieAccount};
use base_tiger_net::{
    BlockBody, BlockSource, ChainHead, ForkSchedule, Network, NetworkConfig, NodeRecord,
    SnapAccount, SnapBatch, SnapHandle, SnapStateSink, SnapSync, SnapSyncConfig, StoreError,
    TouchedKeys,
};
use serde_json::{Value, json};

struct GenesisOnly(ChainHead);

impl BlockSource for GenesisOnly {
    fn chain_head(&self) -> ChainHead {
        self.0
    }
    fn header_by_hash(&self, _: B256) -> Option<Header> {
        None
    }
    fn header_by_number(&self, _: u64) -> Option<Header> {
        None
    }
    fn body_by_hash(&self, _: B256) -> Option<BlockBody> {
        None
    }
    fn receipts_by_hash(&self, _: B256) -> Option<Vec<Bytes>> {
        None
    }
}

/// Flat hashed state in memory, standing in for the store.
#[derive(Default)]
struct Flat {
    accounts: HashMap<B256, SnapAccount>,
    storage: HashMap<B256, BTreeMap<B256, U256>>,
    codes: HashMap<B256, Bytes>,
    progress: Option<Vec<u8>>,
    commits: usize,
    result: Option<(B256, B256)>,
}

#[derive(Clone)]
struct MemSink(Arc<Mutex<Flat>>);

impl Flat {
    fn root(&self) -> B256 {
        let mut accounts: Vec<(&B256, &SnapAccount)> = self.accounts.iter().collect();
        accounts.sort_by_key(|(k, _)| **k);
        let mut builder = HashBuilder::default();
        for (hash, account) in accounts {
            let storage_root = self.storage.get(hash).map_or(EMPTY_ROOT_HASH, |slots| {
                let mut b = HashBuilder::default();
                for (slot, value) in slots {
                    b.add_leaf(Nibbles::unpack(slot), &alloy_rlp::encode(value));
                }
                b.root()
            });
            let trie = TrieAccount {
                nonce: account.nonce,
                balance: account.balance,
                storage_root,
                code_hash: account.code_hash,
            };
            builder.add_leaf(Nibbles::unpack(hash), &alloy_rlp::encode(trie));
        }
        builder.root()
    }
}

impl SnapStateSink for MemSink {
    fn load_progress(&self) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.0.lock().unwrap().progress.clone())
    }

    fn commit_snap(&mut self, batch: &SnapBatch) -> Result<(), StoreError> {
        let mut flat = self.0.lock().unwrap();
        for (hash, account) in &batch.accounts {
            flat.accounts.insert(*hash, *account);
        }
        for (account, slot, value) in &batch.storage {
            flat.storage.entry(*account).or_default().insert(*slot, *value);
        }
        for (hash, code) in &batch.codes {
            flat.codes.insert(*hash, code.clone());
        }
        flat.progress = Some(batch.progress.clone());
        flat.commits += 1;
        Ok(())
    }

    fn has_code(&self, code_hash: B256) -> Result<bool, StoreError> {
        Ok(self.0.lock().unwrap().codes.contains_key(&code_hash))
    }

    fn finish_snap(&mut self, _head: u64, expected_root: B256) -> Result<(), StoreError> {
        let mut flat = self.0.lock().unwrap();
        let root = flat.root();
        flat.result = Some((root, expected_root));
        if root != expected_root {
            return Err(StoreError(format!("state root {root} != expected {expected_root}")));
        }
        Ok(())
    }
}

fn rpc(url: &str, method: &str, params: Value) -> Value {
    let host = url.trim_start_matches("http://");
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut stream = TcpStream::connect(host).unwrap();
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap();
    // Chunked encoding is not used by reth for small bodies; handle it anyway.
    let body = if response.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            rest = &tail[n + 2..];
        }
        out
    } else {
        body.to_string()
    };
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value.get("error").is_none(), "{method}: {value}");
    value["result"].clone()
}

fn hex_u64(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn hex_u256(v: &Value) -> U256 {
    v.as_str().map_or(U256::ZERO, |s| U256::from_str_radix(s.trim_start_matches("0x"), 16).unwrap())
}

/// Applies block `number`'s state changes to the flat state and reports the touched keys.
fn follow_block(url: &str, number: u64, flat: &Arc<Mutex<Flat>>, handle: &SnapHandle) {
    let block = rpc(url, "eth_getBlockByNumber", json!([format!("0x{number:x}"), false]));
    let state_root: B256 = block["stateRoot"].as_str().unwrap().parse().unwrap();
    let timestamp = hex_u64(&block["timestamp"]);
    let traces = rpc(
        url,
        "debug_traceBlockByNumber",
        json!([format!("0x{number:x}"), {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}]),
    );
    let mut touched = TouchedKeys::default();
    let mut flat = flat.lock().unwrap();
    for trace in traces.as_array().unwrap() {
        let result = &trace["result"];
        let pre = result["pre"].as_object().cloned().unwrap_or_default();
        let post = result["post"].as_object().cloned().unwrap_or_default();
        let mut addresses: Vec<&String> = pre.keys().chain(post.keys()).collect();
        addresses.sort();
        addresses.dedup();
        for address in addresses {
            let hashed = keccak256(address.parse::<Address>().unwrap());
            touched.accounts.push(hashed);
            let (before, after) = (pre.get(address), post.get(address));
            if after.is_none() && before.is_some() {
                // In diff mode, an account only in `pre` was deleted.
                flat.accounts.remove(&hashed);
                flat.storage.remove(&hashed);
                touched.wiped.push(hashed);
                continue;
            }
            let field = |name: &str| after.and_then(|a| a.get(name)).or_else(|| before.and_then(|b| b.get(name)));
            let current = flat.accounts.get(&hashed).copied();
            let code_hash = match field("code") {
                Some(code) => keccak256(alloy_primitives::hex::decode(code.as_str().unwrap()).unwrap()),
                None => current.map_or(KECCAK256_EMPTY, |a| a.code_hash),
            };
            let account = SnapAccount {
                nonce: field("nonce").map_or(current.map_or(0, |a| a.nonce), |n| n.as_u64().unwrap()),
                balance: field("balance").map_or(current.map_or(U256::ZERO, |a| a.balance), hex_u256),
                code_hash,
            };
            flat.accounts.insert(hashed, account);
            let mut slots: Vec<String> = Vec::new();
            for side in [before, after].into_iter().flatten() {
                if let Some(storage) = side.get("storage").and_then(|s| s.as_object()) {
                    slots.extend(storage.keys().cloned());
                }
            }
            for slot in slots {
                let key: B256 = slot.parse().unwrap();
                let hashed_slot = keccak256(key);
                let value = after
                    .and_then(|a| a.get("storage"))
                    .and_then(|s| s.get(&slot))
                    .map_or(U256::ZERO, hex_u256);
                touched.storage.push((hashed, hashed_slot));
                let entry = flat.storage.entry(hashed).or_default();
                if value.is_zero() {
                    entry.remove(&hashed_slot);
                } else {
                    entry.insert(hashed_slot, value);
                }
            }
        }
    }
    // System calls before the transactions: EIP-4788 beacon roots and EIP-2935 block hashes.
    let systems = [
        ("0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02", vec![timestamp % 8191, timestamp % 8191 + 8191]),
        ("0x0000F90827F1C53a10cb7A02335B175320002935", vec![(number - 1) % 8191]),
    ];
    for (address, slots) in systems {
        let hashed = keccak256(address.parse::<Address>().unwrap());
        for slot in slots {
            let key = B256::from(U256::from(slot));
            let value = hex_u256(&rpc(url, "eth_getStorageAt", json!([address, format!("{key}"), format!("0x{number:x}")])));
            let hashed_slot = keccak256(key);
            touched.storage.push((hashed, hashed_slot));
            let entry = flat.storage.entry(hashed).or_default();
            if value.is_zero() {
                entry.remove(&hashed_slot);
            } else {
                entry.insert(hashed_slot, value);
            }
        }
    }
    drop(flat);
    handle.on_block(number, state_root, &touched);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut chain_id, mut genesis, mut genesis_time) = (0u64, B256::ZERO, 0u64);
    let mut peers: Vec<NodeRecord> = Vec::new();
    let mut rpc_url: Option<String> = None;
    while let Some(flag) = args.next() {
        let value = args.next().expect("flag value");
        match flag.as_str() {
            "--chain-id" => chain_id = value.parse().unwrap(),
            "--genesis" => genesis = value.parse().unwrap(),
            "--genesis-time" => genesis_time = value.parse().unwrap(),
            "--peer" => peers.push(value.parse().unwrap()),
            "--rpc" => rpc_url = Some(value),
            other => panic!("unknown flag {other}"),
        }
    }
    let forks = ForkSchedule::base(genesis, genesis_time, 0, &[]);
    let mut secret = [0u8; 32];
    std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut secret).unwrap();
    let key = secp256k1::SecretKey::from_byte_array(secret).unwrap();
    let mut config = NetworkConfig::new(key, chain_id, forks);
    config.listen_addr = None;
    config.discovery_addr = None;
    config.trusted_peers = peers;
    let head = ChainHead { number: 0, hash: genesis, timestamp: genesis_time, earliest: 0 };
    let network = Network::start(config, Arc::new(GenesisOnly(head)), None).unwrap();
    let start = Instant::now();
    while network.peers().iter().all(|p| !p.snap) && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let info = network.peers().into_iter().find(|p| p.snap).expect("a snap peer");
    println!("peer {} eth/{} snap={} head={}", info.client_id, info.eth_version, info.snap, info.head_number);

    let flat = Arc::new(Mutex::new(Flat::default()));
    let mut sync = SnapSync::new(network.clone(), MemSink(flat.clone()), SnapSyncConfig::default()).unwrap();
    let handle = sync.handle();
    let follower = match &rpc_url {
        // Follow mode: the node's executed blocks drive the pivot; start a few blocks back so the
        // first downloads happen at a pivot that then ages.
        Some(url) => {
            let url = url.clone();
            let flat = flat.clone();
            let handle = handle.clone();
            let first = hex_u64(&rpc(&url, "eth_blockNumber", json!([])));
            // Seed: the flat state must hold every change after the first pivot, so start the
            // follower at the pivot itself.
            let block = rpc(&url, "eth_getBlockByNumber", json!([format!("0x{first:x}"), false]));
            handle.on_block(first, block["stateRoot"].as_str().unwrap().parse().unwrap(), &TouchedKeys::default());
            Some(std::thread::spawn(move || {
                let mut next = first + 1;
                let mut followed = 0;
                loop {
                    let tip = hex_u64(&rpc(&url, "eth_blockNumber", json!([])));
                    if next > tip {
                        if followed > 0 && flat.lock().unwrap().result.is_some() {
                            return followed;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    follow_block(&url, next, &flat, &handle);
                    next += 1;
                    followed += 1;
                }
            }))
        }
        None => {
            let peer = network.peer(&info.id).unwrap();
            let (number, hash) = peer.head();
            let header = base_tiger_net::Downloader::new(network.clone(), Default::default())
                .headers(base_tiger_net::HeadersRequest {
                    start: base_tiger_net::BlockId::Hash(hash),
                    limit: 1,
                    skip: 0,
                    reverse: false,
                })
                .unwrap()
                .remove(0);
            handle.on_block(number, header.header.state_root, &TouchedKeys::default());
            None
        }
    };
    let t = Instant::now();
    let result = sync.run(&mut |status| println!("{:?} after {:?}", status, t.elapsed()));
    let flat_state = flat.lock().unwrap();
    let slots: usize = flat_state.storage.values().map(BTreeMap::len).sum();
    println!(
        "accounts {} storage slots {} codes {} commits {} in {:?}",
        flat_state.accounts.len(),
        slots,
        flat_state.codes.len(),
        flat_state.commits,
        t.elapsed()
    );
    println!("root check: {:?}", flat_state.result);
    drop(flat_state);
    if let Some(follower) = follower {
        println!("followed {} blocks during the sync", follower.join().unwrap());
    }
    result.expect("snap sync");
    println!("snap sync verified");
    network.shutdown();
}
