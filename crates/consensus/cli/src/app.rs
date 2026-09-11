//! Standalone Base consensus CLI application.

use base_cli_utils::CliStyles;
use clap::{Parser, Subcommand};

use crate::{
    Bootnode, BootnodeEnr, ConsensusChainArgs, ConsensusFollowNodeCommand, ConsensusNodeCommand,
    GlobalConsensusChainArgs,
};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);
base_cli_utils::define_telemetry_args!("BASE_NODE");

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

    /// The standalone binary is what the node containers launch, so telemetry flags reaching
    /// `base-consensus node` is the property that decides whether a deployed node reports at all.
    #[test]
    fn node_command_accepts_telemetry_arguments() {
        let cli = ConsensusCli::parse_from([
            "base-consensus",
            "node",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "--l2-engine-rpc",
            "http://localhost:8551",
            "--telemetry.endpoint",
            "https://telemetry.example.com/v1/ingest",
            "--telemetry.report-interval",
            "120",
        ]);

        let ConsensusCommands::Node(node) = cli.command else {
            panic!("expected the node command");
        };
        assert_eq!(
            node.telemetry.endpoint.map(|endpoint| endpoint.to_string()),
            Some("https://telemetry.example.com/v1/ingest".to_string())
        );
        assert_eq!(node.telemetry.report_interval, 120);
    }

    /// Deployments configure telemetry entirely through the environment, so a rename of these
    /// variables silently disables reporting rather than failing to start.
    #[test]
    fn node_telemetry_arguments_read_the_deployed_environment_variables() {
        let node = ConsensusCli::command()
            .get_subcommands()
            .find(|command| command.get_name() == "node")
            .expect("node subcommand")
            .clone();
        let env_for = |id: &str| {
            node.get_arguments()
                .find(|arg| arg.get_id() == id)
                .and_then(|arg| arg.get_env())
                .map(|env| env.to_string_lossy().into_owned())
        };

        assert_eq!(env_for("telemetry_enabled").as_deref(), Some("BASE_NODE_TELEMETRY_ENABLED"));
        assert_eq!(env_for("telemetry_endpoint").as_deref(), Some("BASE_NODE_TELEMETRY_ENDPOINT"));
        assert_eq!(env_for("telemetry_data_dir").as_deref(), Some("BASE_NODE_TELEMETRY_DATA_DIR"));
        assert_eq!(
            env_for("telemetry_report_interval").as_deref(),
            Some("BASE_NODE_TELEMETRY_REPORT_INTERVAL")
        );
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
