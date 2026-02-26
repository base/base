//! Mempool rebroadcaster binary entry point.

use base_cli_utils::LogConfig;
use clap::Parser;
use dotenvy::dotenv;
use mempool_rebroadcaster::Rebroadcaster;
use tracing::{error, info};

base_cli_utils::define_log_args!("MEMPOOL_REBROADCASTER");

#[derive(Parser, Debug)]
#[command(author, version, about = "A mempool rebroadcaster service")]
struct Args {
    #[arg(long, env, required = true)]
    geth_mempool_endpoint: String,

    #[arg(long, env, required = true)]
    reth_mempool_endpoint: String,

    #[command(flatten)]
    log: LogArgs,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let args = Args::parse();

    LogConfig::from(args.log).init_tracing_subscriber().expect("failed to initialize tracing");

    let rebroadcaster = Rebroadcaster::new(args.geth_mempool_endpoint, args.reth_mempool_endpoint);
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
