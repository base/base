//! CLI definition for the challenger binary.

use clap::Parser;
use eyre::WrapErr;

/// Base Challenger.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_challenger::Cli,
}

impl Cli {
    /// Run the challenger service.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let config = base_challenger::ChallengerConfig::from_cli(self.args)?;
        config.log.init_tracing_subscriber()?;
        config
            .metrics
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                base_challenger::ChallengerMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_cli_utils::RuntimeManager::new()
            .run_until_ctrl_c(base_challenger::ChallengerService::run(config))
    }
}
