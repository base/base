//! CLI definition for the ZK fork dispute binary.

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
        let config = base_zk_fork_dispute::Config::from_cli(self.args).await?;
        config.log.init_tracing_subscriber()?;
        base_zk_fork_dispute::ZkForkDispute::run(config).await
    }
}
