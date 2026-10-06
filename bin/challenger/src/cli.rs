//! CLI definition for the challenger binary.

use clap::Parser;
use eyre::WrapErr;
use reth_node_core::args::TraceArgs;

/// Base Challenger.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_challenger::Cli,

    #[command(flatten)]
    traces: TraceArgs,
}

impl Cli {
    /// Run the challenger service.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { args, traces } = self;
        let logging = args.logging.clone();
        let config = base_challenger::ChallengerConfig::from_cli(args)?;
        config
            .metrics
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                base_challenger::ChallengerMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_cli_utils::RuntimeManager::new().run_until_ctrl_c(async move {
            base_cli_utils::LogConfig::from(logging).init_with_trace_args(&traces, &[])?;
            base_challenger::ChallengerService::run(config).await
        })
    }
}
