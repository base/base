//! Standalone Challenger E2E observation binary for FORT execution.

use base_challenger_e2e::{ChallengerE2e, ChallengerE2eConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_env_filter(EnvFilter::from_default_env()).init();

    if let Err(err) = ChallengerE2e::run(ChallengerE2eConfig::parse()).await {
        tracing::error!(error = %err, error_debug = ?err, "Challenger E2E observation failed");
        std::process::exit(1);
    }

    tracing::info!("Challenger E2E observation passed");
}
