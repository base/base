use std::{env, fs};

use anyhow::Result;
use clap::Parser;
use host_utils::{fetcher::SP1KonaDataFetcher, get_sp1_stdin};
use kona_host::{init_tracing_subscriber, start_server_and_native_client};
use sp1_sdk::ProverClient;

pub const KONA_ELF: &[u8] = include_bytes!("../../elf/riscv32im-succinct-zkvm-elf");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start block number.
    #[arg(short, long)]
    l2_block: u64,

    /// Verbosity level.
    #[arg(short, long, default_value = "0")]
    verbosity: u8,

    /// Skip running native execution.
    #[arg(short, long)]
    use_cache: bool,
}

/// Execute the Kona program for a single block.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();

    let data_fetcher = SP1KonaDataFetcher {
        l2_rpc: env::var("CLABBY_RPC_L2").expect("CLABBY_RPC_L2 is not set."),
        ..Default::default()
    };

    // TODO: Use `optimism_outputAtBlock` to fetch the L2 block at head
    // https://github.com/ethereum-optimism/kona/blob/d9dfff37e2c5aef473f84bf2f28277186040b79f/bin/client/justfile#L26-L32
    let l2_safe_head = args.l2_block - 1;

    let host_cli = data_fetcher
        .get_host_cli_args(l2_safe_head, args.l2_block, args.verbosity)
        .await?;

    let data_dir = host_cli
        .data_dir
        .clone()
        .expect("Data directory is not set.");

    // By default, re-run the native execution unless the user passes `--use-cache`.
    if !args.use_cache {
        // Overwrite existing data directory.
        fs::create_dir_all(&data_dir).unwrap();

        // Initialize the tracer.
        init_tracing_subscriber(host_cli.v).unwrap();
        // Start the server and native client.
        start_server_and_native_client(host_cli.clone())
            .await
            .unwrap();
    }

    // Get the stdin for the block.
    let sp1_stdin = get_sp1_stdin(&host_cli)?;

    let prover = ProverClient::new();
    let (_, report) = prover.execute(KONA_ELF, sp1_stdin).run().unwrap();

    println!(
        "Block {} cycle count: {}",
        args.l2_block,
        report.total_instruction_count()
    );

    Ok(())
}
