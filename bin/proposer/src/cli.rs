//! CLI definition for the proposer binary.

use base_cli_utils::TraceArgs;
use clap::Parser;
use eyre::WrapErr;

/// Base Proposer.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_proposer::Cli,

    #[command(flatten)]
    traces: TraceArgs,
}

impl Cli {
    /// Run the proposer service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let config = base_proposer::ProposerConfig::from_cli(self.args)?;
        config.log.init_with_trace_args(&self.traces, &[])?;
        config
            .metrics
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_proposer::ProposerService::run(config).await
    }
}
