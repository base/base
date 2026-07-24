//! CLI definition for the ZK fork dispute binary.

use base_cli_utils::LogConfig;
use clap::Parser;

/// Base ZK fork dispute.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_zk_fork_dispute::Cli,
}

impl Cli {
    /// Run the fork dispute workflow.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        LogConfig::from(self.args.logging.clone()).init_tracing_subscriber()?;
        let config = base_zk_fork_dispute::Config::from_cli(self.args).await?;
        base_zk_fork_dispute::ZkForkDispute::run(config).await
    }
}
