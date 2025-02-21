use anyhow::Result;
use clap::Parser;

use op_succinct_client_utils::boot::hash_rollup_config;
use op_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RunContext};

#[derive(Parser)]
struct Args {
    /// L2 chain ID
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    dotenv::from_path(args.env_file).ok();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;

    let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
    println!(
        "Rollup Config Hash: 0x{:x}",
        hash_rollup_config(rollup_config)
    );

    Ok(())
}
