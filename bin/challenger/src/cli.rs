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
    pub(crate) fn run(self) -> eyre::Result<()> {
        let config = base_challenger::ChallengerConfig::from_cli(self.args)?;
        base_cli_utils::RuntimeManager::run_until_ctrl_c(
            base_challenger::ChallengerService::run(config),
        )
    }
}
