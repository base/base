//! Integrated execution, builder, and consensus sequencer command.

use std::sync::Arc;

use alloy_provider::RootProvider;
use base_builder_cli::Args as BuilderArgs;
use base_builder_core::{BuilderApiExtension, FlashblocksServiceBuilder};
use base_builder_metering::MeteringStoreExtension;
use base_common_genesis::RollupConfig;
use base_consensus_cli::{
    CliMetrics, ConsensusNodeArgs, ConsensusNodeConfigArgs, ConsensusNodeOverrides,
    EmbeddedSequencerConsensusNodeConfigArgs,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    ExecutionNodeConfigArgs, ExecutionUpgradeSignal, StandardBaseRethNode,
    chainspec::chain_value_parser,
};
use base_node_runner::BaseNodeRunner;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use base_upgrade_signal::{
    UpgradeSignalConfig, UpgradeSignalMetricLayer, UpgradeSignalRuntimeValidation,
    UpgradeSignalStartupMode,
};
use clap::Args;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;
use url::Url;

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
        let Self { execution_chain, execution, mut builder, consensus } = self;
        let mut execution_chain = match execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let consensus_chain = resolved_chain.consensus_chain_args();
        let mut consensus_config: ConsensusNodeConfigArgs = consensus.into();
        Self::derive_execution_upgrade_signal_l1_rpc(&mut builder, &consensus_config);
        consensus_config.upgrade_signal = builder.rollup_args.upgrade_signal.clone();
        let consensus_args = ConsensusNodeArgs::new(consensus_chain, consensus_config);
        let mut rollup_config = consensus_args.load_rollup_config()?;

        let rollup_args = builder.rollup_args.clone();
        let sequencer_rpc = rollup_args.sequencer.clone();
        let metering_provider: base_builder_core::SharedMeteringProvider =
            Arc::new(builder.build_metering_store());
        let builder_config = builder.into_builder_config(Arc::clone(&metering_provider))?;
        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            if let Some(signal_config) = rollup_args.upgrade_signal.config()?
                && signal_config.mode.applies_at_startup()
            {
                let l1_rpc = rollup_args
                    .upgrade_signal_l1_rpc
                    .upgrade_signal_l1_rpc
                    .clone()
                    .expect("upgrade signal L1 RPC is derived before startup");
                Self::apply_shared_startup_upgrade_signal(
                    &mut execution_chain,
                    &mut rollup_config,
                    &signal_config,
                    l1_rpc,
                )
                .await?;
            }

            if metrics_enabled {
                CliMetrics::init_rollup_config(&rollup_config);
            }
            let _upgrade_countdown_metrics = metrics_enabled
                .then(|| CliMetrics::spawn_upgrade_countdown_recorder(rollup_config.clone()));

            let upgrade_signal_runtime_validation =
                Self::upgrade_signal_runtime_validation(&execution_chain);
            let upgrade_signal_l1_rpc =
                rollup_args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.clone();
            let execution =
                execution.into_runtime_config(execution_chain).with_unified_auth_endpoint();
            let l2_engine_rpc = engine_ipc_url(execution.auth_ipc_path())?;

            let task_executor = ctx.task_executor.clone();
            let builder = execution.into_default_node_builder(ctx)?;
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

            let consensus_cancellation = CancellationToken::new();
            let consensus_exit = consensus_args
                .start_with_overrides_and_cancellation_and_upgrade_signal_startup(
                    rollup_config,
                    ConsensusNodeOverrides::embedded_execution(
                        l2_engine_rpc,
                        upgrade_signal_runtime_validation,
                        upgrade_signal_l1_rpc,
                    ),
                    consensus_cancellation.clone(),
                    UpgradeSignalStartupMode::AlreadyApplied,
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

    const fn upgrade_signal_runtime_validation(
        execution_chain: &BaseChainSpec,
    ) -> UpgradeSignalRuntimeValidation {
        UpgradeSignalRuntimeValidation::with_activation_admin_address(
            execution_chain.activation_admin_address,
        )
    }

    async fn apply_shared_startup_upgrade_signal(
        execution_chain: &mut Arc<BaseChainSpec>,
        rollup_config: &mut RollupConfig,
        signal_config: &UpgradeSignalConfig,
        l1_rpc: Url,
    ) -> eyre::Result<()> {
        let reader = signal_config.reader(RootProvider::new_http(l1_rpc));
        let application_schedule = signal_config
            .read_validated_application_schedule(
                &reader,
                "integrated sequencer startup",
                &[UpgradeSignalMetricLayer::Execution, UpgradeSignalMetricLayer::Consensus],
            )
            .await?;

        ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            Arc::make_mut(execution_chain),
            &application_schedule,
        )?;
        ConsensusNodeArgs::apply_schedule_to_rollup_config(rollup_config, &application_schedule);

        Ok(())
    }

    fn derive_execution_upgrade_signal_l1_rpc(
        builder: &mut BuilderArgs,
        consensus_config: &ConsensusNodeConfigArgs,
    ) {
        builder
            .rollup_args
            .upgrade_signal_l1_rpc
            .upgrade_signal_l1_rpc
            .get_or_insert_with(|| consensus_config.l1_rpc_args.l1_eth_rpc.clone());
    }
}

#[cfg(test)]
mod tests {
    use base_consensus_cli::ConsensusNodeConfigArgs;
    use clap::Parser;

    use super::SequencerCommand;
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
    fn parses_upgrade_signal_args() {
        let cli = BaseCli::parse_from(sequencer_args(&[
            "base",
            "sequencer",
            "--p2p.sequencer.key",
            SEQUENCER_KEY,
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.upgrade-id",
            "azul",
        ]));

        let BaseCommand::Sequencer(sequencer) = cli.command else {
            panic!("expected sequencer command");
        };

        assert_eq!(
            sequencer
                .builder
                .rollup_args
                .upgrade_signal
                .contract_address
                .map(|address| address.to_string()),
            Some("0x0000000000000000000000000000000000000001".to_string())
        );
        assert_eq!(sequencer.builder.rollup_args.upgrade_signal.upgrade_ids, ["azul"]);
    }

    #[test]
    fn preserves_explicit_upgrade_signal_l1_rpc() {
        let cli = BaseCli::parse_from(sequencer_args(&[
            "base",
            "sequencer",
            "--p2p.sequencer.key",
            SEQUENCER_KEY,
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.l1-rpc",
            "http://finalized-l1:8545",
        ]));

        let BaseCommand::Sequencer(mut sequencer) = cli.command else {
            panic!("expected sequencer command");
        };
        let consensus_config: ConsensusNodeConfigArgs = sequencer.consensus.clone().into();

        SequencerCommand::derive_execution_upgrade_signal_l1_rpc(
            &mut sequencer.builder,
            &consensus_config,
        );

        assert_eq!(
            sequencer
                .builder
                .rollup_args
                .upgrade_signal_l1_rpc
                .upgrade_signal_l1_rpc
                .as_ref()
                .map(|url| url.as_str()),
            Some("http://finalized-l1:8545/")
        );
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
