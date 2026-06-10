//! Integrated RPC node command.

use std::{path::Path, sync::Arc};

use base_consensus_cli::{
    ConsensusNodeArgs, ConsensusNodeOverrides, EmbeddedConsensusNodeConfigArgs,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, chainspec::chain_value_parser};
use clap::Args;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::ResolvedChainConfig;

/// Arguments for `base rpc`.
#[derive(Args, Clone, Debug)]
#[command(
    mut_arg("builder_disallow", |arg| arg.hide(true).long("__builder-disallow-disabled")),
    mut_arg("sequencer", |arg| arg
        .hide(true)
        .long("__rollup-sequencer-disabled")
        .alias(None::<&'static str>)),
    mut_arg("sequencer_headers", |arg| arg
        .hide(true)
        .long("__rollup-sequencer-headers-disabled")
        .alias(None::<&'static str>))
)]
pub(crate) struct RpcCommand {
    /// Execution chain spec to use instead of the root chain selection.
    #[arg(long = "execution-chain", value_parser = chain_value_parser)]
    pub(crate) execution_chain: Option<Arc<BaseChainSpec>>,

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
        let execution_chain = match self.execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
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
    use std::process::Command;

    use base_execution_chainspec::BaseChainSpec;
    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand, config::ChainArg};

    const RPC_FORWARDING_ENDPOINT_ENV: &str = "OP_RETH_SEQUENCER_HTTP";
    const RPC_FORWARDING_ENDPOINT_ENV_CHILD_TEST: &str =
        "commands::rpc::tests::parses_rpc_forwarding_endpoint_from_env_child";
    const REQUIRED_CONSENSUS_ARGS: &[&str] =
        &["--l1-eth-rpc", "http://localhost:8545", "--l1-beacon", "http://localhost:5052"];

    fn rpc_args(args: &'static [&'static str]) -> Vec<&'static str> {
        let mut full_args = Vec::from(args);
        full_args.extend_from_slice(REQUIRED_CONSENSUS_ARGS);
        full_args
    }

    #[test]
    fn parses_execution_port_and_consensus_rpc_port() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--port",
            "30333",
            "--rpc.port",
            "9546",
        ]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        assert_eq!(rpc.execution.network.port, 30333);
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 9546);
    }

    #[test]
    fn parses_devnet_unified_client_args() {
        let cli = BaseCli::parse_from([
            "base",
            "rpc",
            "--chain",
            "dev",
            "--execution-chain",
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

        assert!(matches!(cli.chain, ChainArg::File(_)));
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        assert_eq!(rpc.execution.rpc.auth_ipc_path, "/data/engine.ipc");
        assert_eq!(rpc.execution.network.port, 30303);
        assert!(rpc.execution_chain.is_some());
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 8549);
        assert_eq!(rpc.consensus.p2p_flags.network.listen_tcp_port, 8003);
    }

    #[test]
    fn parses_rpc_forwarding_endpoint_arg() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--rpc.forwarding-endpoint",
            "http://localhost:8545",
        ]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        let launch_config = rpc.execution.into_launch_config(BaseChainSpec::devnet().into());

        assert_eq!(
            launch_config.standard.rpc.rpc_forwarding_endpoint.as_deref(),
            Some("http://localhost:8545")
        );
        assert_eq!(
            launch_config.standard.rpc.rollup_args.sequencer.as_deref(),
            Some("http://localhost:8545")
        );
        assert!(!launch_config.standard.enable_tx_forwarding);
        assert!(launch_config.standard.builder_rpc_urls.is_empty());
    }

    #[test]
    fn parses_rpc_forwarding_endpoint_from_env() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--exact").arg(RPC_FORWARDING_ENDPOINT_ENV_CHILD_TEST).arg("--ignored");
        command.env(RPC_FORWARDING_ENDPOINT_ENV, "http://localhost:8547");

        let output = command.output().unwrap();

        assert!(
            output.status.success(),
            "child env parsing test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "spawned by parses_rpc_forwarding_endpoint_from_env with isolated process env"]
    fn parses_rpc_forwarding_endpoint_from_env_child() {
        let cli = BaseCli::parse_from(rpc_args(&["base", "rpc"]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        let launch_config = rpc.execution.into_launch_config(BaseChainSpec::devnet().into());

        assert_eq!(
            launch_config.standard.rpc.rpc_forwarding_endpoint.as_deref(),
            Some("http://localhost:8547")
        );
        assert_eq!(
            launch_config.standard.rpc.rollup_args.sequencer.as_deref(),
            Some("http://localhost:8547")
        );
    }

    #[test]
    fn rejects_rpc_mode_arg() {
        let err =
            BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--mode", "sequencer"])).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--mode"));
    }

    #[test]
    fn rejects_rpc_sequencer_args() {
        let err =
            BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--sequencer.stopped"])).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--sequencer.stopped"));
    }

    #[test]
    fn rejects_rpc_conductor_args() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--conductor.rpc",
            "http://localhost:9090",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--conductor.rpc"));
    }

    #[test]
    fn rejects_rpc_builder_args() {
        let err = BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--builder.max-tasks", "1"]))
            .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--builder.max-tasks"));
    }

    #[test]
    fn rejects_rpc_builder_disallow_arg() {
        let err =
            BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--builder.disallow", "deny.json"]))
                .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--builder.disallow"));
    }

    #[test]
    fn rejects_rpc_rollup_sequencer_arg() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--rollup.sequencer",
            "http://localhost:8545",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--rollup.sequencer"));
    }

    #[test]
    fn rejects_rpc_rollup_sequencer_http_alias_arg() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--rollup.sequencer-http",
            "http://localhost:8545",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--rollup.sequencer-http"));
    }

    #[test]
    fn rejects_rpc_rollup_sequencer_ws_alias_arg() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--rollup.sequencer-ws",
            "ws://localhost:8546",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--rollup.sequencer-ws"));
    }

    #[test]
    fn rejects_rpc_rollup_sequencer_headers_arg() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--rollup.sequencer-headers",
            "authorization=token",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--rollup.sequencer-headers"));
    }

    #[test]
    fn rejects_rpc_metering_args() {
        let err =
            BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--enable-metering"])).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--enable-metering"));
    }

    #[test]
    fn rejects_rpc_tx_forwarding_args() {
        let err = BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--enable-tx-forwarding"]))
            .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--enable-tx-forwarding"));
    }

    #[test]
    fn rejects_rpc_p2p_signer_args() {
        let err = BaseCli::try_parse_from(rpc_args(&[
            "base",
            "rpc",
            "--p2p.sequencer.key",
            "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a",
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--p2p.sequencer.key"));
    }
}
