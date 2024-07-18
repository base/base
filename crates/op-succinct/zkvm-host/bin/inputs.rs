use std::env;

use anyhow::Result;
use clap::Parser;
use kona_host::HostCli;
use zkvm_common::BootInfoWithoutRollupConfig;
use zkvm_host::SP1KonaDataFetcher;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Start block number.
    #[arg(short, long)]
    l2_block_number: u64,
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

/// Collect the execution reports across a number of blocks. Inclusive of start and end block.
#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();

    let data_fetcher = SP1KonaDataFetcher {
        l2_rpc: env::var("CLABBY_RPC_L2").unwrap(),
        ..Default::default()
    };

    let native_exec_data = data_fetcher
        .get_native_execution_data(args.l2_block_number)
        .await?;

    let boot_info = from_host_cli_args(&native_exec_data);
    println!("{:?}", boot_info);

    Ok(())
}
