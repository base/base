//! Standalone challenger FORT observer for K8s Job execution.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_env_filter(EnvFilter::from_default_env()).init();

    tracing::info!("starting challenger FORT");

    if let Err(e) = base_challenger_fort::ChallengerFort::run().await {
        tracing::error!(error = %e, error_debug = ?e, "challenger FORT failed");
        std::process::exit(1);
    }

    tracing::info!("challenger FORT passed");
}
