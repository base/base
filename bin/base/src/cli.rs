use clap::{Args, Parser, Subcommand};
use tracing::info;

use crate::config::{ChainArg, ResolvedChainConfig};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

/// The `base` CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = base_cli_utils::CliStyles::init(),
    about,
    long_about = None
)]
pub(crate) struct BaseCli {
    /// Chain selection.
    #[arg(long, short = 'c', global = true, default_value = "mainnet", env = "BASE_CHAIN")]
    pub(crate) chain: ChainArg,

    /// Logging configuration.
    #[command(flatten)]
    pub(crate) logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub(crate) metrics: MetricsArgs,

    /// The command to run.
    #[command(subcommand)]
    pub(crate) command: BaseCommand,
}

/// Top-level commands for `base`.
#[derive(Subcommand, Clone, Debug)]
#[non_exhaustive]
pub(crate) enum BaseCommand {
    /// Start the integrated Base node.
    #[command(name = "node")]
    Node(NodeArgs),
}

impl BaseCommand {
    /// Runs the selected top-level command.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        match self {
            Self::Node(node) => node.run(resolved_chain),
        }
    }
}

/// Arguments for `base node`.
#[derive(Args, Clone, Debug)]
pub(crate) struct NodeArgs {
    /// The node flavor to run.
    #[command(subcommand)]
    pub(crate) command: NodeSubcommand,
}

impl NodeArgs {
    /// Runs the selected `node` subcommand.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        match self.command {
            NodeSubcommand::Rpc(rpc) => rpc.run(resolved_chain),
        }
    }
}

/// Subcommands for `base node`.
#[derive(Subcommand, Clone, Debug)]
pub(crate) enum NodeSubcommand {
    /// Run the integrated node in RPC mode.
    #[command(name = "rpc")]
    Rpc(RpcCommand),
}

/// Arguments for `base node rpc`.
#[derive(Args, Clone, Debug, Default)]
pub(crate) struct RpcCommand;

impl RpcCommand {
    /// Runs the `rpc` flavor.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        info!(chain = ?resolved_chain, "Hello, I'm running this chain");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::BuiltInChain;

    #[test]
    fn parses_default_chain_for_node_rpc() {
        let cli = BaseCli::parse_from(["base", "node", "rpc"]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Mainnet)));
        assert!(matches!(cli.command, BaseCommand::Node(_)));
    }

    #[test]
    fn parses_named_chain_selector() {
        let cli = BaseCli::parse_from(["base", "-c", "sepolia", "node", "rpc"]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_path_chain_selector() {
        let cli = BaseCli::parse_from(["base", "--chain", "./chain.toml", "node", "rpc"]);

        assert!(matches!(cli.chain, ChainArg::File(_)));
    }

    #[test]
    fn chain_arg_uses_base_chain_env_var() {
        let command = BaseCli::command();
        let chain_arg =
            command.get_arguments().find(|arg| arg.get_long() == Some("chain")).unwrap();

        assert_eq!(chain_arg.get_env(), Some(OsStr::new("BASE_CHAIN")));
    }

    #[test]
    fn rejects_multiple_chain_selectors() {
        let err =
            BaseCli::try_parse_from(["base", "-c", "mainnet", "--chain", "sepolia", "node", "rpc"])
                .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used multiple times"));
    }
}
