//! Standalone Base consensus CLI application.

use base_cli_utils::CliStyles;
use clap::{Parser, Subcommand};

use crate::{Bootnode, BootnodeEnr, ConsensusFollowNodeCommand, ConsensusNodeCommand};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    name = "base-consensus",
    version = env!("CARGO_PKG_VERSION"),
    styles = CliStyles::init(),
    about,
    long_about = None
)]
pub struct ConsensusCli {
    /// The command to run.
    #[command(subcommand)]
    pub command: ConsensusCommands,
}

impl ConsensusCli {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        match self.command {
            ConsensusCommands::Node(node) => node.run(),
            ConsensusCommands::Follow(follow) => follow.run(),
            ConsensusCommands::Bootnode(bootnode) => bootnode.run(),
            ConsensusCommands::BootnodeEnr(bootnode_enr) => bootnode_enr.run(),
        }
    }
}

/// Commands for the Base Consensus CLI.
#[derive(Subcommand, Clone, Debug)]
#[expect(clippy::large_enum_variant)]
pub enum ConsensusCommands {
    /// Start the node.
    #[command(name = "node")]
    Node(ConsensusNodeCommand),

    /// Follow another node.
    #[command(name = "follow")]
    Follow(ConsensusFollowNodeCommand),

    /// Start a discovery-only consensus bootnode.
    #[command(name = "bootnode")]
    Bootnode(Bootnode),

    /// Print the deterministic ENR for a consensus bootnode.
    #[command(name = "bootnode-enr")]
    BootnodeEnr(BootnodeEnr),
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn command_name_matches_standalone_binary() {
        assert_eq!(ConsensusCli::command().get_name(), "base-consensus");
    }

    #[test]
    fn parses_bootnode_command() {
        let cli = ConsensusCli::parse_from(["base-consensus", "bootnode"]);

        assert!(matches!(cli.command, ConsensusCommands::Bootnode(_)));
    }

    #[test]
    fn parses_bootnode_enr_command() {
        let cli = ConsensusCli::parse_from(["base-consensus", "bootnode-enr"]);

        assert!(matches!(cli.command, ConsensusCommands::BootnodeEnr(_)));
    }
}
