use anyhow::Result;
use clap::Parser;
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use sp1_sdk::utils;
use std::{
    fs::{self},
    path::PathBuf,
};

/// Write the block data to a CSV file.
fn write_block_data_to_csv(report_path: &PathBuf, block_data: &[BlockInfo]) -> Result<()> {
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut csv_writer = csv::Writer::from_path(report_path)?;

    for block in block_data {
        csv_writer
            .serialize(block)
            .expect("Failed to write execution stats to CSV.");
    }
    csv_writer.flush().expect("Failed to flush CSV writer.");

    Ok(())
}

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
struct BlockDataArgs {
    /// The start block of the range to execute.
    #[clap(long)]
    start: u64,
    /// The end block of the range to execute.
    #[clap(long)]
    end: u64,
    /// The environment file to use.
    #[clap(long, default_value = ".env")]
    env_file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = BlockDataArgs::parse();

    dotenv::from_path(&args.env_file).ok();
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::default();

    let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

    let l2_block_data = data_fetcher
        .get_l2_block_data_range(args.start, args.end)
        .await?;

    // Calculate aggregates
    let total_txns: u64 = l2_block_data.iter().map(|b| b.transaction_count).sum();
    let total_gas: u64 = l2_block_data.iter().map(|b| b.gas_used).sum();
    let total_l1_fees: u128 = l2_block_data.iter().map(|b| b.total_l1_fees).sum();
    let total_tx_fees: u128 = l2_block_data.iter().map(|b| b.total_tx_fees).sum();
    let num_blocks = l2_block_data.len() as u64;

    let report_path = PathBuf::from(format!(
        "block-data/{}/{}-{}-block-data.csv",
        l2_chain_id, args.start, args.end
    ));

    write_block_data_to_csv(&report_path, &l2_block_data)?;
    println!("Wrote block data to {}", report_path.display());

    println!(
        "\nAggregate Block Data for blocks {} to {}:",
        args.start, args.end
    );
    println!("Total Blocks: {}", num_blocks);
    println!("Total Transactions: {}", total_txns);
    println!("Total Gas Used: {}", total_gas);
    println!("Total L1 Fees: {:.6} ETH", total_l1_fees as f64 / 1e18);
    println!("Total TX Fees: {:.6} ETH", total_tx_fees as f64 / 1e18);
    println!(
        "Avg Txns/Block: {:.2}",
        total_txns as f64 / num_blocks as f64
    );
    println!("Avg Gas/Block: {:.2}", total_gas as f64 / num_blocks as f64);
    println!(
        "Avg L1 Fees/Block: {:.6} ETH",
        (total_l1_fees as f64 / num_blocks as f64) / 1e18
    );
    println!(
        "Avg TX Fees/Block: {:.6} ETH",
        (total_tx_fees as f64 / num_blocks as f64) / 1e18
    );
    Ok(())
}
