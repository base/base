//! Command-line entrypoint for replaying captured flashblocks against canonical receipts.

use std::path::PathBuf;

use alloy_primitives::B256;
use base_flashblocks::{
    DEFAULT_PROCESSOR_LIKE_MAX_DEPTH, ReplayEventScenario, ReplayRequest, replay_capture,
};
use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EventScenarioArg {
    Captured,
    InjectParentCanonicalAfterFlashblock,
    InjectCurrentCanonicalAfterFlashblock,
    InjectParentAndCurrentCanonicalAfterFlashblock,
}

impl From<EventScenarioArg> for ReplayEventScenario {
    fn from(value: EventScenarioArg) -> Self {
        match value {
            EventScenarioArg::Captured => Self::Captured,
            EventScenarioArg::InjectParentCanonicalAfterFlashblock => {
                Self::InjectParentCanonicalAfterFlashblock
            }
            EventScenarioArg::InjectCurrentCanonicalAfterFlashblock => {
                Self::InjectCurrentCanonicalAfterFlashblock
            }
            EventScenarioArg::InjectParentAndCurrentCanonicalAfterFlashblock => {
                Self::InjectParentAndCurrentCanonicalAfterFlashblock
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "base-flashblocks-replay")]
#[command(about = "Replay captured flashblocks against canonical receipts")]
struct Args {
    #[arg(long, env = "FLASHBLOCKS_CAPTURE_DIR")]
    capture_dir: PathBuf,

    #[arg(long, env = "BASE_RPC_URL")]
    rpc_url: String,

    #[arg(long)]
    block_number: u64,

    #[arg(long)]
    start_block_number: Option<u64>,

    #[arg(long, default_value_t = DEFAULT_PROCESSOR_LIKE_MAX_DEPTH)]
    max_pending_blocks_depth: u64,

    #[arg(long, value_delimiter = ',')]
    window_block_counts: Vec<u64>,

    #[arg(long, value_delimiter = ',', value_enum)]
    event_scenarios: Vec<EventScenarioArg>,

    #[arg(long, value_delimiter = ',')]
    max_pending_blocks_depths: Vec<u64>,

    #[arg(long, default_value_t = 8)]
    parallelism: usize,

    #[arg(long)]
    trace_tx_hash: Option<B256>,

    #[arg(long)]
    trace_output_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let start_block_number =
        args.start_block_number.or_else(|| Some(args.block_number.saturating_sub(3)));
    let request = ReplayRequest {
        capture_dir: args.capture_dir,
        rpc_url: args.rpc_url,
        start_block_number,
        block_number: args.block_number,
        max_pending_blocks_depth: args.max_pending_blocks_depth,
        window_block_counts: args.window_block_counts,
        event_scenarios: args.event_scenarios.into_iter().map(Into::into).collect(),
        max_pending_blocks_depths: args.max_pending_blocks_depths,
        parallelism: args.parallelism,
        trace_tx_hash: args.trace_tx_hash,
        trace_output_dir: args.trace_output_dir,
    };

    let summaries = replay_capture(request).await?;
    println!("{}", serde_json::to_string_pretty(&summaries)?);
    Ok(())
}
