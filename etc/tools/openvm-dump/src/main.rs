#![doc = include_str!("../README.md")]

use std::path::PathBuf;

use base_openvm_dump::OpenVmRangeDump;
use clap::Parser;
use eyre::Result;
use tracing_subscriber::EnvFilter;
use url::Url;

/// Dump a 1-block `OpenVM` range witness from live L1/L2 RPCs.
#[derive(Debug, Parser)]
#[command(name = "openvm-dump")]
struct Args {
    /// L1 execution RPC URL.
    #[arg(long, env = "OPENVM_L1_RPC")]
    l1_rpc: Url,
    /// L1 beacon HTTP URL.
    #[arg(long, env = "OPENVM_L1_BEACON_RPC")]
    l1_beacon_rpc: Url,
    /// L2 execution RPC URL.
    #[arg(long, env = "OPENVM_L2_RPC")]
    l2_rpc: Url,
    /// L2 consensus (op-node) RPC URL.
    #[arg(long, env = "OPENVM_L2_NODE_RPC")]
    l2_node_rpc: Url,
    /// Inclusive start block. Defaults to `end_block - 1`.
    #[arg(long, env = "OPENVM_START_BLOCK")]
    start_block: Option<u64>,
    /// Inclusive end block. Defaults to the current safe L2 head.
    #[arg(long, env = "OPENVM_END_BLOCK")]
    end_block: Option<u64>,
    /// Directory that receives `input.json`.
    #[arg(long)]
    out_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // Derivation logs one WARN per historical batch; a live zeronet
            // block will emit millions of "Dropping old batch" lines.
            EnvFilter::new("info,batch_validator=off,batch_queue=off")
        }))
        .try_init();

    let args = Args::parse();
    OpenVmRangeDump {
        l1_rpc: args.l1_rpc,
        l1_beacon_rpc: args.l1_beacon_rpc,
        l2_rpc: args.l2_rpc,
        l2_node_rpc: args.l2_node_rpc,
        start_block: args.start_block,
        end_block: args.end_block,
        out_dir: args.out_dir,
    }
    .dump()
    .await
}
