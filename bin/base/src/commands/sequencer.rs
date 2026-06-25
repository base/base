//! Integrated execution, builder, and consensus sequencer command.

use std::sync::Arc;

use base_builder_cli::Args as BuilderArgs;
use base_builder_core::{BuilderApiExtension, FlashblocksServiceBuilder};
use base_builder_metering::MeteringStoreExtension;
use base_consensus_cli::{
    CliMetrics, ConsensusNodeArgs, ConsensusNodeOverrides, EmbeddedSequencerConsensusNodeConfigArgs,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    ExecutionNodeConfigArgs, StandardBaseRethNode, chainspec::chain_value_parser,
};
use base_node_runner::BaseNodeRunner;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use clap::Args;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;

use crate::{commands::rpc::engine_ipc_url, config::ResolvedChainConfig};

/// Arguments for `base sequencer`.
#[derive(Args, Clone, Debug)]
pub(crate) struct SequencerCommand {
    /// Execution chain spec to use instead of the root chain selection.
    #[arg(long = "execution-chain", value_parser = chain_value_parser)]
    pub(crate) execution_chain: Option<Arc<BaseChainSpec>>,

    /// Embedded execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeConfigArgs,

    /// Embedded builder and Flashblocks arguments.
    #[command(flatten)]
    pub(crate) builder: BuilderArgs,

    /// Embedded consensus sequencer arguments.
    #[command(flatten)]
    pub(crate) consensus: EmbeddedSequencerConsensusNodeConfigArgs,
}

impl SequencerCommand {
    /// Runs the `sequencer` flavor with execution, builder, and consensus in one process.
    pub(crate) fn run(
        self,
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        let execution_chain = match self.execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let consensus_chain = resolved_chain.consensus_chain_args();
        let consensus_args = ConsensusNodeArgs::new(consensus_chain, self.consensus.into());
        let rollup_config = consensus_args.load_rollup_config()?;
        if metrics_enabled {
            CliMetrics::init_rollup_config(&rollup_config);
        }

        let rollup_args = self.builder.rollup_args.clone();
        let sequencer_rpc = rollup_args.sequencer.clone();
        let metering_provider: base_builder_core::SharedMeteringProvider =
            Arc::new(self.builder.build_metering_store());
        let builder_config = self.builder.into_builder_config(Arc::clone(&metering_provider))?;
        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();

        let execution =
            self.execution.into_runtime_config(execution_chain).with_unified_auth_endpoint();
        let l2_engine_rpc = engine_ipc_url(execution.auth_ipc_path())?;

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            let _upgrade_countdown_metrics = metrics_enabled
                .then(|| CliMetrics::spawn_upgrade_countdown_recorder(rollup_config.clone()));
            let task_executor = ctx.task_executor.clone();
            let builder = execution.into_default_node_builder(ctx)?;
            // Execution upgrade-signal polling remains independently configured from consensus,
            // so this path still relies on an explicit `--upgrade-signal.l1-rpc` when enabled.
            let builder = StandardBaseRethNode::apply_initial_upgrade_signal_from_rollup_args(
                builder,
                &rollup_args,
            )
            .await?;
            let mut runner = BaseNodeRunner::new(rollup_args.clone())
                .with_da_config(da_config)
                .with_gas_limit_config(gas_limit_config)
                .with_service_builder(FlashblocksServiceBuilder(builder_config));
            runner.install_ext::<MeteringStoreExtension>(metering_provider);
            runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig { sequencer_rpc });
            runner.install_ext::<BuilderApiExtension>(());
            StandardBaseRethNode::install_upgrade_signal_metrics_extension(
                &mut runner,
                &rollup_args,
            )?;

            let launched = runner.launch(builder).await?;
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand};

    const REQUIRED_CONSENSUS_ARGS: &[&str] =
        &["--l1-eth-rpc", "http://localhost:8545", "--l1-beacon", "http://localhost:5052"];
    const SEQUENCER_KEY: &str = "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a";

    fn sequencer_args(args: &'static [&'static str]) -> Vec<&'static str> {
        let mut full_args = Vec::from(args);
        full_args.extend_from_slice(REQUIRED_CONSENSUS_ARGS);
        full_args
    }

    #[test]
    fn parses_execution_consensus_builder_and_sequencer_args() {
        let cli = BaseCli::parse_from(sequencer_args(&[
            "base",
            "sequencer",
            "--port",
            "30333",
            "--rpc.port",
            "9546",
            "--builder.max_gas_per_txn",
            "30000000",
            "--flashblocks.port",
            "1112",
            "--rollup.sequencer",
            "http://localhost:8545",
            "--rollup.sequencer-headers",
            "Authorization: Bearer token",
            "--sequencer.stopped",
            "--conductor.rpc",
            "http://localhost:9090",
            "--p2p.sequencer.key",
            SEQUENCER_KEY,
        ]));

        let BaseCommand::Sequencer(sequencer) = cli.command else {
            panic!("expected sequencer command");
        };

        assert_eq!(sequencer.execution.network.port, 30333);
        assert_eq!(sequencer.consensus.rpc_flags.listen_port, 9546);
        assert_eq!(sequencer.builder.max_gas_per_txn, Some(30_000_000));
        assert_eq!(sequencer.builder.flashblocks.flashblocks_port, 1112);
        assert_eq!(
            sequencer.builder.rollup_args.sequencer.as_deref(),
            Some("http://localhost:8545")
        );
        assert_eq!(
            sequencer.builder.rollup_args.sequencer_headers,
            vec!["Authorization: Bearer token"]
        );
        assert!(sequencer.consensus.sequencer_flags.stopped);
        assert_eq!(
            sequencer.consensus.sequencer_flags.conductor_rpc.as_ref().map(url::Url::as_str),
            Some("http://localhost:9090/")
        );
        assert!(sequencer.consensus.p2p_flags.signer.sequencer_key.is_some());
    }

    #[test]
    fn parses_without_l2_engine_rpc() {
        let cli = BaseCli::parse_from(sequencer_args(&[
            "base",
            "sequencer",
            "--p2p.sequencer.key",
            SEQUENCER_KEY,
        ]));

        assert!(matches!(cli.command, BaseCommand::Sequencer(_)));
    }

    #[test]
    fn rejects_sequencer_mode_arg() {
        let err = BaseCli::try_parse_from(sequencer_args(&[
            "base",
            "sequencer",
            "--mode",
            "sequencer",
            "--p2p.sequencer.key",
            SEQUENCER_KEY,
        ]))
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("--mode"));
    }
}
