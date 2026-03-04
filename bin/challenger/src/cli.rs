//! CLI definition for the challenger binary.

use clap::Parser;

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
    pub(crate) async fn run(self) -> eyre::Result<()> {
        base_challenger::ChallengerService::run(base_challenger::ChallengerConfig::from_cli(self.args)?).await
    }
}
