//! Standalone SNARK Groth16 E2E test binary for K8s `CronJob` execution.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_env_filter(EnvFilter::from_default_env()).init();

    tracing::info!("starting SNARK Groth16 E2E test");

    if let Err(e) = base_zk_service::SnarkE2e::run().await {
        tracing::error!(error = %e, "SNARK E2E test failed");
        std::process::exit(1);
    }

    tracing::info!("SNARK Groth16 E2E test passed");
}
