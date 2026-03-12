#![doc = include_str!("../README.md")]

use std::path::PathBuf;

use clap::Parser;
use eyre::Result;

/// Compare two execution witnesses to find trie node differences.
///
/// Fetches one witness from an L2 RPC via `debug_executionWitness` and reads
/// the other from a local JSON file, then reports exactly which trie nodes,
/// codes, and keys differ — including a detailed breakdown of affected accounts.
#[derive(Parser, Debug)]
#[command(name = "witness-diff")]
struct Args {
    /// Path to the local witness JSON file (from re-execution).
    #[arg(long)]
    local: PathBuf,

    /// L2 RPC URL to fetch the reference witness from.
    #[arg(long)]
    rpc_url: String,

    /// Block number to fetch the witness for.
    #[arg(long)]
    block: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    base_witness_diff::run(args.local, args.rpc_url, args.block).await
}
