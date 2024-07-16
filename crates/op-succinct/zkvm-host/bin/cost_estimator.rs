use anyhow::Result;
use clap::Parser;
use kona_host::HostCli;
use native_host::run_native_host;
use zkvm_common::BootInfoWithoutRollupConfig;
use zkvm_host::{execute_kona_program, SP1KonaDataFetcher};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start block number
    #[arg(short, long)]
    start_block: u64,

    /// End block number
    #[arg(short, long)]
    end_block: u64,
}

fn from_host_cli_args(args: &HostCli) -> BootInfoWithoutRollupConfig {
    BootInfoWithoutRollupConfig {
        l1_head: args.l1_head,
        l2_output_root: args.l2_output_root,
        l2_claim: args.l2_claim,
        l2_claim_block: args.l2_block_number,
        chain_id: args.l2_chain_id,
    }
}

/// Collect the execution reports across a number of blocks.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut reports = Vec::new();

    let data_fetcher = SP1KonaDataFetcher::new();

    for block_num in args.start_block..=args.end_block {
        // Get native execution data.
        let native_execution_data = data_fetcher.get_native_execution_data(block_num).await?;

        // Run the native host to generate the merkle proofs.
        run_native_host(&native_execution_data).await?;

        // Execute Kona program and collect execution report
        let boot_info = from_host_cli_args(&native_execution_data);
        let report = execute_kona_program(&boot_info);

        println!("Block {}: {}", block_num, report);

        reports.push(report);
    }

    Ok(())
}
