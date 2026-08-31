//! Standalone challenger E2E binary for K8s `CronJob` execution.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_env_filter(EnvFilter::from_default_env()).init();

    tracing::info!("starting challenger E2E test");

    if let Err(e) = base_challenger_e2e::ChallengerE2e::run().await {
        tracing::error!(error = %e, error_debug = ?e, "challenger E2E test failed");
        std::process::exit(1);
    }

    tracing::info!("challenger E2E test passed");
}
