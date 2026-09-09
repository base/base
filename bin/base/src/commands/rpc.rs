//! Integrated RPC node command.

use std::sync::Arc;

use base_cli_utils::CliRunner;
use base_consensus_cli::{
    CliMetrics, ConsensusFollowNodeArgs, ConsensusNodeArgs, ConsensusNodeConfigArgs,
    ConsensusNodeOverrides, ConsensusNodeStartOptions, EmbeddedConsensusNodeConfigArgs,
    EmbeddedFollowArgs,
};
use base_consensus_engine::LocalEngineClient;
use base_consensus_providers::{L1RpcProvider, LocalL2Provider};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, chainspec::chain_value_parser};
use base_upgrade_signal::UpgradeSignalStartupMode;
use clap::Args;
use tokio_util::sync::CancellationToken;

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

    /// Optional source-based synchronization for proofs nodes.
    #[command(flatten)]
    pub follow: EmbeddedFollowArgs,
}

impl RpcCommand {
    /// Runs the `rpc` flavor.
    pub(crate) fn run(
        self,
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        let mut execution_chain = match self.execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let consensus_chain = resolved_chain.consensus_chain_args();
        let mut execution = self.execution;
        let mut consensus_config: ConsensusNodeConfigArgs = self.consensus.into();
        execution
            .standard
            .rollup_args
            .upgrade_signal_l1_rpc
            .apply_default_from(&consensus_config.l1_rpc_args.l1_eth_rpc);
        consensus_config.upgrade_signal = execution.standard.rollup_args.upgrade_signal.clone();
        let consensus_args = ConsensusNodeArgs::new(consensus_chain, consensus_config);
        let mut rollup_config = consensus_args.load_rollup_config()?;

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            execution
                .standard
                .rollup_args
                .upgrade_signal
                .apply_startup_to_sinks(
                    &execution.standard.rollup_args.upgrade_signal_l1_rpc,
                    "integrated startup",
                    execution_chain.chain().id(),
                    Arc::make_mut(&mut execution_chain),
                    &mut rollup_config,
                )
                .await?;

            if metrics_enabled {
                CliMetrics::init_rollup_config(&rollup_config);
            }
            let _upgrade_countdown_metrics = metrics_enabled
                .then(|| CliMetrics::spawn_upgrade_countdown_recorder(rollup_config.clone()));

            let upgrade_signal_l1_rpc =
                execution.standard.rollup_args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.clone();
            let execution = execution
                .into_launch_config(execution_chain)
                .with_upgrade_signal_startup_already_applied();

            let task_executor = ctx.task_executor.clone();
            let launched = execution.launch(ctx).await?;
            let handle = launched;
            // Keep the execution node handle alive until both services have coordinated shutdown.
            let execution_node = handle.node;
            let execution_exit = handle.node_exit_future;
            let execution_client = LocalEngineClient {
                l1: L1RpcProvider::new_http_with_timeout(
                    consensus_args.config.l1_rpc_args.l1_eth_rpc.clone(),
                    consensus_args.config.l1_rpc_args.l1_rpc_timeout,
                ),
                l2: LocalL2Provider { provider: execution_node.provider.clone(), rollup_config: Arc::new(rollup_config.clone()) },
                execution: execution_node.execution.clone(),
                network: execution_node.network.clone(),
                proofs_progress: execution_node.proofs_progress.get().cloned(),
            };


            let consensus_cancellation = CancellationToken::new();
            let follow_config = self.follow.into_config(consensus_args.config.clone());
            let consensus_exit = async {
                if let Some(config) = follow_config {
                    let follow_args =
                        ConsensusFollowNodeArgs::new(consensus_args.chain.clone(), config);
                    return tokio::select! {
                        result = follow_args.start_with_rollup_config(rollup_config, execution_client.clone()) => result,
                        _ = consensus_cancellation.cancelled() => Ok(()),
                    };
                }
                consensus_args
                    .start_with_options(
                        ConsensusNodeStartOptions::new(rollup_config)
                            .with_overrides(ConsensusNodeOverrides::embedded_execution(
                                execution_client,
                                upgrade_signal_l1_rpc,
                            ))
                            .with_cancellation(consensus_cancellation.clone())
                            .with_upgrade_signal_startup_mode(
                                UpgradeSignalStartupMode::AlreadyApplied,
                            ),
                    )
                    .await
            };
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

#[cfg(test)]
mod tests {
    use std::process::Command;

    use base_consensus_cli::ConsensusNodeConfigArgs;
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
    fn follow_mode_preserves_source_and_proofs_options() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--http",
            "--source-l2-rpc=http://source:8545",
            "--follow.proofs",
            "--proofs.max-blocks-ahead=8",
            "--follow.insert-delay-ms=25",
        ]));
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };
        let config = rpc.follow.into_config(rpc.consensus.into()).unwrap();
        assert_eq!(config.source_l2_rpc.as_str(), "http://source:8545/");
        assert!(config.proofs);
        assert_eq!(config.proofs_max_blocks_ahead, 8);
        assert_eq!(config.insert_delay.as_millis(), 25);
    }

    #[test]
    fn follow_mode_uses_native_execution_and_proofs_require_source() {
        BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--source-l2-rpc=http://source:8545"]))
            .unwrap();
        let error =
            BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--follow.proofs"])).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);
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

        assert_eq!(rpc.execution.node.network.port, 30333);
        assert_eq!(rpc.consensus.rpc_flags.listen_port, 9546);
    }

    #[test]
    fn parses_upgrade_signal_args_once() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
        ]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        assert_eq!(
            rpc.execution
                .standard
                .rollup_args
                .upgrade_signal
                .contract_address
                .map(|address| address.to_string()),
            Some("0x0000000000000000000000000000000000000001".to_string())
        );
    }

    #[test]
    fn derives_upgrade_signal_l1_rpc_from_integrated_consensus_args() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
        ]));

        let BaseCommand::Rpc(mut rpc) = cli.command else {
            panic!("expected rpc command");
        };
        let consensus_config: ConsensusNodeConfigArgs = rpc.consensus.clone().into();

        rpc.execution
            .standard
            .rollup_args
            .upgrade_signal_l1_rpc
            .apply_default_from(&consensus_config.l1_rpc_args.l1_eth_rpc);

        assert_eq!(
            rpc.execution
                .standard
                .rollup_args
                .upgrade_signal_l1_rpc
                .upgrade_signal_l1_rpc
                .as_ref()
                .map(|url| url.as_str()),
            Some("http://localhost:8545/")
        );
    }

    #[test]
    fn preserves_explicit_upgrade_signal_l1_rpc() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.l1-rpc",
            "http://finalized-l1:8545",
        ]));

        let BaseCommand::Rpc(mut rpc) = cli.command else {
            panic!("expected rpc command");
        };
        let consensus_config: ConsensusNodeConfigArgs = rpc.consensus.clone().into();

        rpc.execution
            .standard
            .rollup_args
            .upgrade_signal_l1_rpc
            .apply_default_from(&consensus_config.l1_rpc_args.l1_eth_rpc);

        assert_eq!(
            rpc.execution
                .standard
                .rollup_args
                .upgrade_signal_l1_rpc
                .upgrade_signal_l1_rpc
                .as_ref()
                .map(|url| url.as_str()),
            Some("http://finalized-l1:8545/")
        );
    }

    #[test]
    fn parses_devnet_unified_client_args() {
        let cli = BaseCli::parse_from([
            "base",
            "--chain",
            "dev",
            "rpc",
            "--execution-chain",
            "dev",
            "--datadir=/data",
            "--http",
            "--http.addr=0.0.0.0",
            "--http.port=8545",
            "--ws",
            "--ws.addr=0.0.0.0",
            "--ws.port=8546",
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

        assert!(matches!(cli.chain, Some(ChainArg::BuiltIn(ref name)) if name == "dev"));
        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        assert_eq!(rpc.execution.node.network.port, 30303);
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
        assert!(!launch_config.standard.rpc.enable_tx_forwarding);
        assert!(launch_config.standard.rpc.builder_rpc_urls.is_empty());
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
    fn parses_rpc_metering_args() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--enable-metering",
            "--metering.metered-opcodes",
            "SSTORE",
        ]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        let launch_config = rpc.execution.into_launch_config(BaseChainSpec::devnet().into());

        assert!(launch_config.standard.metering.enable_metering);
        assert_eq!(
            launch_config.standard.metering.metering_metered_opcodes,
            vec!["SSTORE".to_string()]
        );
    }

    #[test]
    fn parses_rpc_validity_forwarding_args() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--enable-tx-forwarding",
            "--builder-rpc-urls",
            "http://localhost:8545",
            "--enable-experimental-validity-transactions",
            "--experimental-validity-max-predicates",
            "8",
        ]));

        let BaseCommand::Rpc(rpc) = cli.command else {
            panic!("expected rpc command");
        };

        let launch_config = rpc.execution.into_launch_config(BaseChainSpec::devnet().into());

        assert!(launch_config.standard.rpc.enable_tx_forwarding);
        assert!(launch_config.standard.rpc.enable_experimental_validity_transactions);
        assert_eq!(launch_config.standard.rpc.experimental_validity_max_predicates, 8);
        assert_eq!(launch_config.standard.rpc.builder_rpc_urls.len(), 1);
    }

    #[test]
    fn rpc_tx_forwarding_requires_builder_urls() {
        let err = BaseCli::try_parse_from(rpc_args(&["base", "rpc", "--enable-tx-forwarding"]))
            .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--builder-rpc-urls"));
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
