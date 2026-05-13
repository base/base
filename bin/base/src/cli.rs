use std::path::Path;

use base_consensus_cli::{
    ConsensusNodeArgs, ConsensusNodeOverrides, EmbeddedConsensusNodeConfigArgs,
};
use base_execution_cli::ExecutionNodeArgs;
use clap::{Args, Parser, Subcommand};
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;
use url::Url;

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
#[derive(Args, Clone, Debug)]
pub(crate) struct RpcCommand {
    /// Execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeArgs,

    /// Consensus node arguments.
    #[command(flatten)]
    pub(crate) consensus: EmbeddedConsensusNodeConfigArgs,
}

impl RpcCommand {
    /// Runs the `rpc` flavor.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        let execution_chain = resolved_chain.execution_chain_spec()?;
        let consensus_chain = resolved_chain.consensus_chain_args();
        let consensus_args = ConsensusNodeArgs::new(consensus_chain, self.consensus.into());
        let rollup_config = consensus_args.load_rollup_config()?;

        let execution = self.execution.into_launch_config(execution_chain).with_auth_ipc();
        let l2_engine_rpc = engine_ipc_url(execution.auth_ipc_path())?;

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            let task_executor = ctx.task_executor.clone();
            let launched = execution.launch_default(ctx).await?;
            let handle = launched.handle;
            // Keep the execution node handle alive until both services have coordinated shutdown.
            let execution_node = handle.node;
            let execution_exit = handle.node_exit_future;

            let overrides = ConsensusNodeOverrides {
                l2_engine_rpc: Some(l2_engine_rpc),
                l2_engine_jwt_secret: None,
            };

            let consensus_cancellation = CancellationToken::new();
            let consensus_exit = consensus_args.start_with_overrides_and_cancellation(
                rollup_config,
                overrides,
                consensus_cancellation.clone(),
            );
            tokio::pin!(execution_exit);
            tokio::pin!(consensus_exit);

            let result = tokio::select! {
                result = &mut execution_exit => {
                    consensus_cancellation.cancel();
                    let consensus_result = consensus_exit.await;
                    result?;
                    consensus_result
                }
                result = &mut consensus_exit => {
                    let consensus_result = result;
                    task_executor
                        .initiate_graceful_shutdown()
                        .map_err(|e| eyre::eyre!("failed to signal execution node shutdown: {e}"))?
                        .ignore_guard()
                        .await;
                    let execution_result = execution_exit.await;
                    consensus_result?;
                    execution_result
                }
            };

            drop(execution_node);
            result
        })
    }
}

fn engine_ipc_url(path: &str) -> eyre::Result<Url> {
    let path = Path::new(path);
    let path =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    Url::from_file_path(&path).map_err(|()| {
        eyre::eyre!("failed to convert auth IPC path to file URL: {}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::BuiltInChain;

    const REQUIRED_CONSENSUS_ARGS: &[&str] =
        &["--l1-eth-rpc", "http://localhost:8545", "--l1-beacon", "http://localhost:5052"];

    fn node_rpc_args(args: &'static [&'static str]) -> Vec<&'static str> {
        let mut full_args = Vec::from(args);
        full_args.extend_from_slice(REQUIRED_CONSENSUS_ARGS);
        full_args
    }

    #[test]
    fn parses_default_chain_for_node_rpc() {
        let cli = BaseCli::parse_from(node_rpc_args(&["base", "node", "rpc"]));

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Mainnet)));
        assert!(matches!(cli.command, BaseCommand::Node(_)));
    }

    #[test]
    fn parses_named_chain_selector() {
        let cli = BaseCli::parse_from(node_rpc_args(&["base", "-c", "sepolia", "node", "rpc"]));

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_global_chain_after_rpc_subcommand() {
        let cli =
            BaseCli::parse_from(node_rpc_args(&["base", "node", "rpc", "--chain", "sepolia"]));

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_path_chain_selector() {
        let cli =
            BaseCli::parse_from(node_rpc_args(&["base", "--chain", "./chain.toml", "node", "rpc"]));

        assert!(matches!(cli.chain, ChainArg::File(_)));
    }

    #[test]
    fn parses_execution_port_and_consensus_rpc_port() {
        let cli = BaseCli::parse_from(node_rpc_args(&[
            "base",
            "node",
            "rpc",
            "--port",
            "30333",
            "--rpc.port",
            "9546",
        ]));

        let BaseCommand::Node(node) = cli.command;
        let NodeSubcommand::Rpc(rpc) = node.command;

        assert_eq!(rpc.execution.network.port, 30333);
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 9546);
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
        let err = BaseCli::try_parse_from(node_rpc_args(&[
            "base", "-c", "mainnet", "--chain", "sepolia", "node", "rpc",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used multiple times"));
    }
}
