use clap::Parser;
use dotenvy::dotenv;
use tracing::{error, info, Level};
use tracing_subscriber::EnvFilter;

use mempool_rebroadcaster::rebroadcaster::Rebroadcaster;

#[derive(Parser, Debug)]
#[command(author, version, about = "A mempool rebroadcaster service")]
struct Args {
    #[arg(long, env, required = true)]
    geth_mempool_endpoint: String,

    #[arg(long, env, required = true)]
    reth_mempool_endpoint: String,

    #[arg(long, env, default_value = "info")]
    log_level: Level,

    /// Format for logs, can be json or text
    #[arg(long, env, default_value = "text")]
    log_format: String,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let args = Args::parse();

    let log_format = args.log_format.to_lowercase();
    let log_level = args.log_level.to_string();

    if log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::new(log_level))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(log_level))
            .with_ansi(false)
            .init();
    }

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
