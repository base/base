//! Standalone Base consensus CLI application.

use base_cli_utils::CliStyles;
use clap::{Parser, Subcommand};

use crate::{
    Bootnode, BootnodeEnr, ConsensusChainArgs, ConsensusFollowNodeCommand, ConsensusNodeCommand,
    GlobalConsensusChainArgs,
};

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
    /// Chain selection.
    #[command(flatten)]
    pub chain: GlobalConsensusChainArgs,

    /// The command to run.
    #[command(subcommand)]
    pub command: ConsensusCommands,
}

impl ConsensusCli {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        let chain = ConsensusChainArgs::from(self.chain);
        match self.command {
            ConsensusCommands::Node(node) => node.run(chain),
            ConsensusCommands::Follow(follow) => follow.run(chain),
            ConsensusCommands::Bootnode(bootnode) => bootnode.run(chain),
            ConsensusCommands::BootnodeEnr(bootnode_enr) => bootnode_enr.run(chain),
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

    #[test]
    fn parses_global_chain_before_command() {
        let cli = ConsensusCli::parse_from(["base-consensus", "--chain", "84532", "bootnode"]);

        assert_eq!(cli.chain.l2_chain_id, alloy_chains::Chain::from(84532_u64));
    }

    #[test]
    fn parses_global_chain_after_command() {
        let cli = ConsensusCli::parse_from(["base-consensus", "bootnode", "--chain", "84532"]);

        assert_eq!(cli.chain.l2_chain_id, alloy_chains::Chain::from(84532_u64));
    }
}
