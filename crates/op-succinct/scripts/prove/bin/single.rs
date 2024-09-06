use std::env;

use anyhow::Result;
use clap::Parser;
use num_format::{Locale, ToFormattedString};
use op_succinct_host_utils::{
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_proof_stdin,
    witnessgen::WitnessGenExecutor,
    ProgramType,
};
use sp1_sdk::{utils, ProverClient};

pub const SINGLE_BLOCK_ELF: &[u8] = include_bytes!("../../../elf/fault-proof-elf");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start block number.
    #[arg(short, long)]
    l2_block: u64,

    /// Skip running native execution.
    #[arg(short, long)]
    use_cache: bool,
}

/// Execute the OP Succinct program for a single block.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher {
        l2_rpc: env::var("L2_RPC").expect("L2_RPC is not set."),
        ..Default::default()
    };

    let l2_safe_head = args.l2_block - 1;

    let cache_mode = if args.use_cache { CacheMode::KeepCache } else { CacheMode::DeleteCache };

    let host_cli = data_fetcher
        .get_host_cli_args(l2_safe_head, args.l2_block, ProgramType::Single, cache_mode)
        .await?;

    // By default, re-run the native execution unless the user passes `--use-cache`.
    if !args.use_cache {
        // Start the server and native client.
        let mut witnessgen_executor = WitnessGenExecutor::default();
        witnessgen_executor.spawn_witnessgen(&host_cli).await?;
        witnessgen_executor.flush().await?;
    }

    // Get the stdin for the block.
    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = ProverClient::new();
    let (_, report) = prover.execute(SINGLE_BLOCK_ELF, sp1_stdin).run().unwrap();

    println!(
        "Block {} cycle count: {}",
        args.l2_block,
        report.total_instruction_count().to_formatted_string(&Locale::en)
    );

    Ok(())
}
