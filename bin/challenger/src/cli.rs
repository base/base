//! CLI definition for the challenger binary.

use clap::Parser;
use eyre::WrapErr;

/// Base Challenger.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_proof_service_challenger::Cli,
}

impl Cli {
    /// Run the challenger service.
    pub(crate) fn run(self) -> eyre::Result<()> {
        base_common_cli_support::LogConfig::from(self.args.logging.clone())
            .init_tracing_subscriber()?;
        let config = base_proof_service_challenger::ChallengerConfig::from_cli(self.args)?;
        config
            .metrics
            .init_with(|| {
                base_common_cli_support::register_version_metrics!();
                base_proof_service_challenger::ChallengerMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_common_cli_support::RuntimeManager::new()
            .run_until_ctrl_c(base_proof_service_challenger::ChallengerService::run(config))
    }
}
