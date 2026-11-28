//! Times a block's state root (`StateView::prepare_root`) on a synthetic state, over the
//! datadir alone and over an in-memory layer of unpersisted blocks, as the node computes it when
//! validating a block. Run from `tiger/`:
//!
//! ```sh
//! cargo run --release -p base-tiger-trie --example root_bench -- /tmp/root-bench
//! ```
//!
//! The datadir is built on the first run and reused. Sizes come from the environment:
//! `ACCOUNTS` (default 2,000,000), `CONTRACTS` (20,000 with up to 64 slots each), `BIG` (4 contracts
//! with 400,000 slots each), `MEM_BLOCKS` (64 blocks in memory), `BLOCKS` (20 timed blocks), and
//! the block shape: `TOUCHED` accounts (3,000), `STORAGE_ACCOUNTS` (150) with `SLOTS` changes each
//! on average (12), `BIG_TOUCH` slots in each big contract (200), and `REUSE`, the percent of
//! touches that pick a recently touched account or contract again (0). Mainnet-shaped blocks
//! (Base, November 2026): `TOUCHED=24 STORAGE_ACCOUNTS=24 SLOTS=2 BIG_TOUCH=8 REUSE=60`.

use std::{sync::Arc, time::Instant};

use alloy_primitives::{Address, U256, keccak256};
use base_tiger_trie::{AccountInfo, BlockChanges, MemState, StateDb};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn address(kind: u8, index: u64) -> Address {
    Address::from_slice(&keccak256([[kind].as_slice(), &index.to_be_bytes()].concat())[12..])
}

fn info(rng: &mut Rng, code: bool) -> AccountInfo {
    let code_hash =
        if code { keccak256(rng.next().to_be_bytes()) } else { alloy_trie::KECCAK_EMPTY };
    AccountInfo { nonce: rng.below(1000), balance: U256::from(rng.next()), code_hash }
}

struct Sizes {
    accounts: u64,
    contracts: u64,
    big: u64,
    big_slots: u64,
}

fn slot_value(rng: &mut Rng) -> U256 {
    U256::from(rng.next() >> rng.below(64))
}

/// Recently touched accounts and contracts, which `REUSE` percent of touches pick again.
#[derive(Default)]
struct Recent {
    accounts: Vec<u64>,
    contracts: Vec<u64>,
}

/// One of `n` indices: a recent one (`REUSE` percent of the time), or any.
fn pick(rng: &mut Rng, recent: &mut Vec<u64>, n: u64) -> u64 {
    let index = if !recent.is_empty() && rng.below(100) < env("REUSE", 0) {
        recent[rng.below(recent.len() as u64) as usize]
    } else {
        rng.below(n)
    };
    recent.push(index);
    if recent.len() > 2000 {
        recent.drain(..1000);
    }
    index
}

fn block(rng: &mut Rng, sizes: &Sizes, recent: &mut Recent) -> BlockChanges {
    let mut changes = BlockChanges::default();
    for _ in 0..env("TOUCHED", 3000) {
        let index = pick(rng, &mut recent.accounts, sizes.accounts + 1000);
        changes.accounts.push((address(0, index), Some(info(rng, false))));
    }
    for _ in 0..env("STORAGE_ACCOUNTS", 150) {
        let contract = address(1, pick(rng, &mut recent.contracts, sizes.contracts));
        changes.accounts.push((contract, Some(info(rng, true))));
        for _ in 0..1 + rng.below(2 * env("SLOTS", 12)) {
            let value = if rng.below(5) == 0 { U256::ZERO } else { slot_value(rng) };
            changes.storage.push((contract, U256::from(rng.below(96)), value));
        }
    }
    for big in 0..sizes.big {
        let contract = address(2, big);
        for _ in 0..env("BIG_TOUCH", 200) {
            let value = if rng.below(8) == 0 { U256::ZERO } else { slot_value(rng) };
            changes.storage.push((
                contract,
                U256::from(rng.below(sizes.big_slots * 11 / 10)),
                value,
            ));
        }
    }
    changes
}

fn genesis(db: &StateDb, sizes: &Sizes) {
    let mut rng = Rng(1);
    let mut changes = BlockChanges::default();
    for index in 0..sizes.accounts {
        changes.accounts.push((address(0, index), Some(info(&mut rng, false))));
    }
    for index in 0..sizes.contracts {
        let contract = address(1, index);
        changes.accounts.push((contract, Some(info(&mut rng, true))));
        for slot in 0..rng.below(64) {
            changes.storage.push((
                contract,
                U256::from(slot),
                slot_value(&mut rng) + U256::from(1),
            ));
        }
    }
    for big in 0..sizes.big {
        let contract = address(2, big);
        changes.accounts.push((contract, Some(info(&mut rng, true))));
        for slot in 0..sizes.big_slots {
            changes.storage.push((
                contract,
                U256::from(slot),
                slot_value(&mut rng) + U256::from(1),
            ));
        }
    }
    let started = Instant::now();
    let mut w = db.write().unwrap();
    let root = db.commit_hashed(&mut w, 0, &changes.hashed()).unwrap();
    w.commit().unwrap();
    println!("genesis {root} in {:.1} s", started.elapsed().as_secs_f64());
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn report(name: &str, mut times: Vec<f64>) {
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "{name}: mean {mean:.2} ms  p50 {:.2}  p90 {:.2}  max {:.2}  ({} blocks)",
        percentile(&times, 0.5),
        percentile(&times, 0.9),
        percentile(&times, 1.0),
        times.len()
    );
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/root-bench".into());
    let sizes = Sizes {
        accounts: env("ACCOUNTS", 2_000_000),
        contracts: env("CONTRACTS", 20_000),
        big: env("BIG", 4),
        big_slots: env("BIG_SLOTS", 400_000),
    };
    let fresh = !std::path::Path::new(&path).exists();
    let db = StateDb::open(std::path::Path::new(&path)).unwrap();
    if fresh {
        genesis(&db, &sizes);
    }
    let mut rng = Rng(7);
    let mut recent = Recent::default();
    let blocks: Vec<BlockChanges> =
        (0..env("BLOCKS", 20)).map(|_| block(&mut rng, &sizes, &mut recent)).collect();
    let mut digest = Vec::new();

    let chain_only = std::env::var("CHAIN_ONLY").is_ok();
    let mut times = Vec::new();
    for changes in blocks.iter().filter(|_| !chain_only) {
        let view = db.view(0, Vec::new()).unwrap();
        let started = Instant::now();
        let prepared = view.prepare_root(changes).unwrap();
        times.push(started.elapsed().as_secs_f64() * 1e3);
        digest.extend_from_slice(prepared.root.as_slice());
        digest.extend_from_slice(format!("{:?}", prepared.trie_updates()).as_bytes());
    }
    if !chain_only {
        report("datadir", times);
    }

    let mut mem = MemState::new(0);
    let started = Instant::now();
    for number in 1..=env("MEM_BLOCKS", 64) {
        let changes = block(&mut rng, &sizes, &mut recent);
        let view = db.view_mem(Arc::new(mem.clone()), Vec::new()).unwrap();
        let prepared = view.prepare_root(&changes).unwrap();
        mem.extend(number, prepared.hashed(), prepared.trie_updates().unwrap(), &[]);
    }
    println!(
        "memory layer of {} blocks built in {:.1} s",
        mem.head,
        started.elapsed().as_secs_f64()
    );
    let mem = Arc::new(mem);
    let mut mem_digest = Vec::new();
    let mut times = Vec::new();
    for changes in blocks.iter().filter(|_| !chain_only) {
        let view = db.view_mem(Arc::clone(&mem), Vec::new()).unwrap();
        let started = Instant::now();
        let prepared = view.prepare_root(changes).unwrap();
        times.push(started.elapsed().as_secs_f64() * 1e3);
        mem_digest.extend_from_slice(prepared.root.as_slice());
        mem_digest.extend_from_slice(format!("{:?}", prepared.trie_updates()).as_bytes());
    }
    if !chain_only {
        report("memory layer", times);
    }

    digest.extend_from_slice(&mem_digest);
    println!("digest of roots and trie updates: {}", keccak256(&digest));

    // A chain: each block over the one before, as a following node validates them.
    let mut chain = (*mem).clone();
    let mut chain_digest = Vec::new();
    let mut times = Vec::new();
    for changes in &blocks {
        let view = db.view_mem(Arc::new(chain.clone()), Vec::new()).unwrap();
        let started = Instant::now();
        let prepared = view.prepare_root(changes).unwrap();
        times.push(started.elapsed().as_secs_f64() * 1e3);
        chain_digest.extend_from_slice(prepared.root.as_slice());
        chain.extend(chain.head + 1, prepared.hashed(), prepared.trie_updates().unwrap(), &[]);
    }
    println!("chain first block: {:.2} ms", times[0]);
    report("chain", times[1..].to_vec());
    println!("digest of chain roots: {}", keccak256(&chain_digest));
}
