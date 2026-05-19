//! CLI definition for the challenger v2 binary.

use clap::Parser;
use eyre::WrapErr;

/// Base Challenger v2.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_challenger_v2::Cli,
}

impl Cli {
    /// Run the challenger service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let config = base_challenger_v2::ChallengerConfig::from_cli(self.args)?;
        config.log.init_tracing_subscriber()?;
        config
            .metrics
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_challenger_v2::ChallengerService::run(config).await
    }
}
