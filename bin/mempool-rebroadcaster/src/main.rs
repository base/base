//! Mempool rebroadcaster binary entry point.

mod cli;

use base_cli_utils::LogConfig;
use clap::Parser;
use cli::Args;
use dotenvy::dotenv;
use mempool_rebroadcaster::{Rebroadcaster, RebroadcasterConfig};
use tracing::{error, info};

base_cli_utils::define_log_args!("MEMPOOL_REBROADCASTER");

#[tokio::main]
async fn main() {
    dotenv().ok();
    let args = Args::parse();

    LogConfig::from(args.log.clone()).init_tracing_subscriber().expect("failed to initialize tracing");

    let config = RebroadcasterConfig::from(args);
    let rebroadcaster = Rebroadcaster::new(config);
    let result = rebroadcaster.run().await;

    match result {
        Ok(result) => {
            info!(
                success_geth_to_reth = result.success_geth_to_reth,
                success_reth_to_geth = result.success_reth_to_geth,
                unexpected_failed_geth_to_reth = result.unexpected_failed_geth_to_reth,
                unexpected_failed_reth_to_geth = result.unexpected_failed_reth_to_geth,
                "finished broadcasting txns",
            );
        }
        Err(e) => {
            error!(error = ?e, "error running rebroadcaster");
            std::process::exit(1);
        }
    }
}
