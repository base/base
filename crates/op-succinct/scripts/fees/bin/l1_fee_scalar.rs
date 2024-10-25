use std::time::Instant;

use alloy_primitives::U256;
use anyhow::Result;
use clap::Parser;
use op_succinct_fees::aggregate_fee_data;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;

#[derive(Parser)]
struct Args {
    #[clap(long)]
    start: u64,
    #[clap(long)]
    end: u64,
    #[clap(long, default_value = None)]
    l1_fee_scalar: Option<U256>,
    #[clap(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();
    let fetcher = OPSuccinctDataFetcher::default();

    let start_time = Instant::now();
    let (fee_data, modified_fee_data) = tokio::join!(
        fetcher.get_l2_fee_data_range(args.start, args.end),
        fetcher.get_l2_fee_data_with_modified_l1_fee_scalar(
            args.start,
            args.end,
            args.l1_fee_scalar
        )
    );
    let duration = start_time.elapsed();
    println!("Done getting fee data. Time taken: {:?}", duration);

    let fee_data = fee_data?;
    let modified_fee_data = modified_fee_data?;

    let total_aggregate_fee_data = aggregate_fee_data(fee_data)?;
    let modified_total_aggregate_fee_data = aggregate_fee_data(modified_fee_data)?;

    println!("{modified_total_aggregate_fee_data}");
    println!("{total_aggregate_fee_data}");

    assert_eq!(
        total_aggregate_fee_data.total_l1_fee,
        modified_total_aggregate_fee_data.total_l1_fee
    );

    println!("Success!");

    Ok(())
}
