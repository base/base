//! Connects tiger's network stack to peers, exchanges Status, and downloads and validates the
//! last N blocks' headers and bodies. Used for interop tests against base-reth.
//!
//! ```text
//! cargo run --release --example probe -- --chain-id 84538453 --genesis <hash> \
//!     [--genesis-time <t>] [--fork-times t1,t2,...] [--peer enode://...]... \
//!     [--bootnode enode://...]... [--discovery 0.0.0.0:30399] [--blocks 1000] [--wait 60]
//! ```

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes};
use base_tiger_net::{
    BlockBody, BlockSource, ChainHead, DownloadConfig, Downloader, ForkSchedule, Network,
    NetworkConfig, NodeRecord,
};

/// Serves nothing: the probe starts at genesis.
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

fn main() {
    let mut args = std::env::args().skip(1);
    let mut chain_id = 0u64;
    let mut genesis = B256::ZERO;
    let mut genesis_time = 0u64;
    let mut fork_times: Vec<u64> = Vec::new();
    let mut peers: Vec<NodeRecord> = Vec::new();
    let mut bootnodes: Vec<NodeRecord> = Vec::new();
    let mut discovery = None;
    let mut blocks = 1000u64;
    let mut wait = 60u64;
    let mut min_peers = 1usize;
    while let Some(flag) = args.next() {
        let value = args.next().expect("flag value");
        match flag.as_str() {
            "--chain-id" => chain_id = value.parse().unwrap(),
            "--genesis" => genesis = value.parse().unwrap(),
            "--genesis-time" => genesis_time = value.parse().unwrap(),
            "--fork-times" => fork_times = value.split(',').map(|t| t.parse().unwrap()).collect(),
            "--peer" => peers.push(value.parse().unwrap()),
            "--bootnode" => bootnodes.push(value.parse().unwrap()),
            "--discovery" => discovery = Some(value.parse().unwrap()),
            "--blocks" => blocks = value.parse().unwrap(),
            "--wait" => wait = value.parse().unwrap(),
            "--min-peers" => min_peers = value.parse().unwrap(),
            other => panic!("unknown flag {other}"),
        }
    }
    let forks = ForkSchedule::base(genesis, genesis_time, 0, &fork_times);
    let mut secret = [0u8; 32];
    getrandom_fill(&mut secret);
    let key = secp256k1::SecretKey::from_byte_array(secret).unwrap();
    let mut config = NetworkConfig::new(key, chain_id, forks);
    config.listen_addr = None;
    config.discovery_addr = discovery;
    config.trusted_peers = peers;
    config.bootnodes = bootnodes;
    let head = ChainHead { number: 0, hash: genesis, timestamp: genesis_time, earliest: 0 };
    let network = Network::start(config, Arc::new(GenesisOnly(head)), None).unwrap();
    println!("local id {}", network.local_id());
    if let Some(enr) = network.local_enr() {
        println!("local enr {enr}");
    }
    let start = Instant::now();
    while network.peer_count() < min_peers && start.elapsed() < Duration::from_secs(wait) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let peers = network.peers();
    assert!(!peers.is_empty(), "no peer connected within {wait}s");
    for peer in &peers {
        println!(
            "peer {} {} client={} eth/{} snap={} head={}",
            peer.id, peer.addr, peer.client_id, peer.eth_version, peer.snap, peer.head_number
        );
    }
    let best = peers.iter().max_by_key(|p| p.head_number).unwrap();
    let peer = network.peer(&best.id).unwrap();
    let (head_number, head_hash) = peer.head();
    println!("connected in {:?}; peer head {head_number} {head_hash}", start.elapsed());

    let downloader = Downloader::new(network.clone(), DownloadConfig::default());
    let t = Instant::now();
    let stop_at = head_number.saturating_sub(blocks - 1);
    let headers = downloader.headers_back(head_hash, stop_at, blocks).expect("headers");
    let elapsed = t.elapsed();
    assert_eq!(headers[0].hash, head_hash);
    assert!(headers.windows(2).all(|w| w[0].header.parent_hash == w[1].hash));
    println!(
        "headers: {} ({}..={}) linked by hash in {:?}",
        headers.len(),
        headers.last().unwrap().number(),
        headers[0].number(),
        elapsed
    );
    let t = Instant::now();
    let bodies = downloader.bodies(&headers, &mut |_| {}).expect("bodies");
    let txs: usize = bodies.iter().map(|b| b.transactions.len()).sum();
    println!(
        "bodies: {} with {txs} transactions, roots checked, in {:?}",
        bodies.len(),
        t.elapsed()
    );
    for header in headers.iter().step_by((headers.len() / 4).max(1)) {
        println!("block {} {}", header.number(), header.hash);
    }
    network.shutdown();
}

fn getrandom_fill(buf: &mut [u8]) {
    use std::io::Read;
    std::fs::File::open("/dev/urandom").unwrap().read_exact(buf).unwrap();
}
