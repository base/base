//! CLI definition for the proposer binary.

use clap::Parser;

/// Base Proposer.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct ProposerBin {
    #[command(flatten)]
    args: base_proposer::Cli,
}

impl ProposerBin {
    /// Run the proposer service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        base_proposer::run(base_proposer::ProposerConfig::from_cli(self.args)?).await
    }
}
