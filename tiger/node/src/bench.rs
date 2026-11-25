//! `tiger bench`: replays real blocks through the node's validation path on a full datadir and
//! reports where the time goes, for measuring optimizations on a fixed workload.
//!
//! - `tiger bench fetch --rpc <url> --from <n> --count <n> --out <file>` saves canonical blocks
//!   (`debug_getRawBlock`) to a file, each as a little-endian `u32` length and the block's RLP.
//! - `tiger bench replay --chain <chain> --datadir <dir> --blocks <file> [--count <n>]
//!   [--no-prewarm] [--no-cache]` validates the blocks after the datadir's tip in order, the
//!   way the Engine API does on a full node (prewarm, the cross-block execution cache, the
//!   in-memory layer), checks each state root, makes each block canonical in memory, and prints
//!   per-phase percentiles. It never persists: the datadir is opened but not written, so the
//!   same datadir replays the same blocks again (with a warmer page cache).

use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::Bytes;
use base_common_consensus::BaseTxEnvelope as SignedTx;
use serde_json::json;

use crate::{BlockExecution, Genesis, Node, Upstream, Validation};

/// Blocks per `debug_getRawBlock` batch.
const FETCH_BATCH: u64 = 50;

/// Runs `tiger bench <args>`.
pub fn run(args: &[String]) -> Result<(), String> {
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    let value = |name: &str| -> Option<&str> {
        args.iter().position(|arg| arg == name).and_then(|index| args.get(index + 1)).map(String::as_str)
    };
    let required = |name: &str| value(name).ok_or_else(|| format!("tiger bench needs {name}"));
    let number = |name: &str| -> Result<u64, String> {
        required(name)?.parse().map_err(|error| format!("{name}: {error}"))
    };
    match args.first().map(String::as_str) {
        Some("fetch") => fetch(required("--rpc")?, number("--from")?, number("--count")?, required("--out")?),
        Some("replay") => replay(&Replay {
            chain: required("--chain")?.to_owned(),
            datadir: PathBuf::from(required("--datadir")?),
            blocks: PathBuf::from(required("--blocks")?),
            count: value("--count").map(str::parse).transpose().map_err(|error| format!("--count: {error}"))?,
            prewarm: !flag("--no-prewarm"),
            cache: !flag("--no-cache"),
        }),
        _ => Err("usage: tiger bench fetch|replay (see node/src/bench.rs)".into()),
    }
}

fn fetch(rpc: &str, from: u64, count: u64, out: &str) -> Result<(), String> {
    let upstream = Upstream::new(rpc)?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(out).map_err(|error| format!("{out}: {error}"))?);
    let mut next = from;
    while next < from + count {
        let batch: Vec<u64> = (next..(next + FETCH_BATCH).min(from + count)).collect();
        let calls: Vec<_> = batch.iter().map(|n| ("debug_getRawBlock", json!([format!("{n:#x}")]))).collect();
        for (n, raw) in batch.iter().zip(upstream.call_batch(&calls)?) {
            let raw: Bytes = serde_json::from_value(raw).map_err(|error| format!("block {n}: {error}"))?;
            let len = u32::try_from(raw.len()).map_err(|_| format!("block {n} is too large"))?;
            file.write_all(&len.to_le_bytes()).and_then(|()| file.write_all(&raw)).map_err(|error| error.to_string())?;
        }
        next += batch.len() as u64;
        eprintln!("tiger bench: fetched up to block {}", next - 1);
    }
    file.flush().map_err(|error| error.to_string())
}

struct Replay {
    chain: String,
    datadir: PathBuf,
    blocks: PathBuf,
    count: Option<usize>,
    prewarm: bool,
    cache: bool,
}

/// One replayed block's timings.
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// Validation minus roots: decoding, checks and execution (with prewarm running beside it).
    exec: Duration,
    /// State and storage roots.
    root: Duration,
    /// Storing the block and making it canonical in memory.
    canonical: Duration,
    gas: u64,
}

fn read_blocks(path: &PathBuf) -> Result<Vec<alloy_consensus::Block<SignedTx>>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|mut file| file.read_to_end(&mut bytes)).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut blocks = Vec::new();
    let mut rest = bytes.as_slice();
    while !rest.is_empty() {
        let (len, tail) = rest.split_at_checked(4).ok_or("truncated blocks file")?;
        let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
        let (raw, tail) = tail.split_at_checked(len).ok_or("truncated blocks file")?;
        blocks.push(alloy_rlp::decode_exact(raw).map_err(|error| format!("block {}: {error}", blocks.len()))?);
        rest = tail;
    }
    Ok(blocks)
}

fn replay(options: &Replay) -> Result<(), String> {
    let blocks = read_blocks(&options.blocks)?;
    let node = Node::open(&options.datadir, Genesis::load(&options.chain)?, None)?;
    {
        let mut chain = node.chain.write().expect("chain lock");
        chain.set_persistence(u64::MAX, 0)?;
        chain.set_exec_cache(options.cache);
    }
    let tip = node.chain.read().expect("chain lock").head().number();
    let todo: Vec<_> = blocks.into_iter().filter(|block| block.header.number > tip).take(options.count.unwrap_or(usize::MAX)).collect();
    let first = todo.first().ok_or_else(|| format!("no blocks after the datadir tip {tip}"))?.header.number;
    if first != tip + 1 {
        return Err(format!("the blocks start at {first}, the datadir tip is {tip}"));
    }
    eprintln!(
        "tiger bench: replaying {} blocks from {first} (prewarm {}, execution cache {})",
        todo.len(),
        options.prewarm,
        options.cache
    );
    let started = Instant::now();
    let mut timings = Vec::with_capacity(todo.len());
    for block in todo {
        let header = block.header;
        let hash = header.hash_slow();
        let transactions: Vec<Bytes> = block.body.transactions.iter().map(|tx| tx.encoded_2718().into()).collect();
        let chain = node.chain.read().expect("chain lock");
        let parent = chain.block(&header.parent_hash).ok_or_else(|| format!("block {} has no parent", header.number))?;
        let state = chain.state_after(&parent)?;
        let validate = || BlockExecution { chain: &node.spec }.validate(&header, &transactions, &parent.header, &state);
        let validation_started = Instant::now();
        let validated = if options.prewarm {
            crate::prewarm::warm_while(&chain, &node.spec, &header, &transactions, &parent, validate)
        } else {
            validate()
        }?;
        let (validation, root) = (validation_started.elapsed(), state.root_time());
        if let Some(invalid) = validated.invalid {
            return Err(format!("block {} {hash} is invalid: {invalid}", header.number));
        }
        let mut executed = validated.executed.ok_or("a valid block executed")?;
        let prepared = state.take_prepared().map(|prepared| Validation::Prepared(Box::new(prepared)));
        drop(state);
        drop(chain);
        let canonical_started = Instant::now();
        let gas = header.gas_used;
        executed.header = header;
        let mut chain = node.chain.write().expect("chain lock");
        chain.insert(hash, &executed, prepared)?;
        chain.set_canonical(hash)?;
        drop(chain);
        timings.push(Timing { exec: validation - root, root, canonical: canonical_started.elapsed(), gas });
    }
    let elapsed = started.elapsed();
    let gas: u64 = timings.iter().map(|t| t.gas).sum();
    println!(
        "blocks {} in {:.2}s: {:.1} blocks/s, {:.0} Mgas/s",
        timings.len(),
        elapsed.as_secs_f64(),
        timings.len() as f64 / elapsed.as_secs_f64(),
        gas as f64 / elapsed.as_secs_f64() / 1e6
    );
    for (name, pick) in [
        ("validate", (|t: &Timing| t.exec + t.root) as fn(&Timing) -> Duration),
        ("exec", |t: &Timing| t.exec),
        ("root", |t: &Timing| t.root),
        ("canonical", |t: &Timing| t.canonical),
    ] {
        let mut values: Vec<f64> = timings.iter().map(|t| pick(t).as_secs_f64() * 1e3).collect();
        values.sort_by(f64::total_cmp);
        let at = |p: f64| values[((values.len() as f64 * p) as usize).min(values.len() - 1)];
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        println!(
            "{name:>9} ms: mean {mean:7.2}  p50 {:7.2}  p90 {:7.2}  p99 {:7.2}  max {:7.2}",
            at(0.5),
            at(0.9),
            at(0.99),
            values[values.len() - 1]
        );
    }
    Ok(())
}
