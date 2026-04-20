use base_cli_utils::{LogConfig, MetricsConfig};
use eyre::WrapErr;

use crate::{cli::BaseCli, config::ChainResolver};

/// Runs the `base` binary.
#[derive(Debug, Clone)]
pub(crate) struct BaseApp {
    /// Parsed CLI input.
    pub cli: BaseCli,
}

impl BaseApp {
    /// Creates a new app from parsed CLI input.
    pub(crate) const fn new(cli: BaseCli) -> Self {
        Self { cli }
    }

    /// Runs the requested command.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let BaseCli { chain, logging, metrics, command } = self.cli;

        LogConfig::from(logging)
            .init_tracing_subscriber()
            .wrap_err("failed to initialize tracing")?;

        MetricsConfig::from(metrics)
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;

        let resolved_chain = ChainResolver::new(chain).resolve()?;
        command.run(resolved_chain)
    }
}
