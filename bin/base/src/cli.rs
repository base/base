use std::path::Path;

use base_consensus_cli::{
    ConsensusNodeArgs, ConsensusNodeOverrides, EmbeddedConsensusNodeConfigArgs,
};
use base_execution_cli::ExecutionNodeArgs;
use clap::{Args, Parser, Subcommand};
use reth_cli_runner::CliRunner;
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
    #[arg(long, short = 'c', global = true, default_value = "base", env = "BASE_CHAIN")]
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
            let launched = execution.launch_default(ctx).await?;
            let handle = launched.handle;
            let _execution_node = handle.node;
            let execution_exit = handle.node_exit_future;

            let overrides = ConsensusNodeOverrides {
                l2_engine_rpc: Some(l2_engine_rpc),
                l2_engine_jwt_secret: None,
            };

            tokio::select! {
                result = execution_exit => result,
                result = consensus_args.start_with_overrides(rollup_config, overrides) => {
                    result.map_err(|e| eyre::eyre!(e))
                }
            }
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
        let cli =
            BaseCli::parse_from(node_rpc_args(&["base", "-c", "base-sepolia", "node", "rpc"]));

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
    }

    #[test]
    fn parses_global_chain_after_rpc_subcommand() {
        let cli = BaseCli::parse_from(node_rpc_args(&["base", "node", "rpc", "--chain", "dev"]));

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Dev)));
    }

    #[test]
    fn parses_legacy_short_chain_alias() {
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
    fn parses_devnet_unified_client_args() {
        let cli = BaseCli::parse_from([
            "base",
            "node",
            "rpc",
            "--chain",
            "dev",
            "--datadir=/data",
            "--http",
            "--http.addr=0.0.0.0",
            "--http.port=8545",
            "--ws",
            "--ws.addr=0.0.0.0",
            "--ws.port=8546",
            "--authrpc.port=8551",
            "--authrpc.addr=0.0.0.0",
            "--authrpc.jwtsecret=/genesis/jwt.hex",
            "--auth-ipc.path=/data/engine.ipc",
            "--port=30303",
            "--discovery.port=30303",
            "--metrics=0.0.0.0:8090",
            "--txpool.nolocals",
            "--rollup.txpool-max-inflight-delegated-slots=32768",
            "--txpool.pending-max-count=200000",
            "--txpool.pending-max-size=512",
            "--txpool.basefee-max-count=200000",
            "--txpool.basefee-max-size=512",
            "--txpool.queued-max-count=200000",
            "--txpool.queued-max-size=512",
            "--txpool.max-account-slots=256",
            "--txpool.max-batch-size=1024",
            "--rpc.txfeecap=0",
            "--rpc.gascap=600000000",
            "--rpc.eth-proof-window=1209600",
            "--flashblocks-url=ws://base-builder:7111",
            "--bootnodes=enode://4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1@172.30.0.10:9303",
            "--rollup.discovery.v4",
            "--rollup.sequencer=http://base-builder:7545",
            "--enable-metering",
            "--metering.gas-limit=60000000",
            "--metering.execution-time-us=5000000",
            "--metering.state-root-time-us=1000000",
            "--metering.da-bytes=1572860",
            "--metering.target-flashblocks-per-block=10",
            "--l1-eth-rpc",
            "http://l1-el:8545",
            "--l1-beacon",
            "http://l1-cl:5052",
            "--l2-config-file",
            "/genesis/l2/rollup.json",
            "--l1-config-file",
            "/genesis/el/chain-config.json",
            "--l1-slot-duration-override",
            "4",
            "--rpc.addr",
            "0.0.0.0",
            "--rpc.port",
            "8549",
            "--p2p.listen.tcp",
            "8003",
            "--p2p.listen.udp",
            "8003",
            "--p2p.advertise.ip",
            "127.0.0.1",
            "--p2p.bootnodes-file",
            "/bootnodes/enr.txt",
            "--p2p.scoring",
            "Off",
            "--l1.verifier-confs",
            "15",
            "-vvv",
        ]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Dev)));
        let BaseCommand::Node(node) = cli.command;
        let NodeSubcommand::Rpc(rpc) = node.command;

        assert_eq!(rpc.execution.rpc.auth_ipc_path, "/data/engine.ipc");
        assert_eq!(rpc.execution.network.port, 30303);
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 8549);
        assert_eq!(rpc.consensus.p2p_flags.listen_tcp_port, 8003);
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
