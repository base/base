//! Integrated RPC node command.

use std::{path::Path, sync::Arc};

use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_consensus_cli::{
    ConsensusNodeArgs, ConsensusNodeConfigArgs, ConsensusNodeOverrides,
    EmbeddedConsensusNodeConfigArgs,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    ExecutionNodeArgs, ExecutionUpgradeSignal, chainspec::chain_value_parser,
};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalMetrics, UpgradeSignalStartupMode,
};
use clap::Args;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

use crate::config::ResolvedChainConfig;

/// Arguments for `base rpc`.
#[derive(Args, Clone, Debug)]
#[command(
    mut_arg("builder_disallow", |arg| arg.hide(true).long("__builder-disallow-disabled")),
    mut_arg("sequencer", |arg| arg.hide(true).long("__rollup-sequencer-disabled")),
    mut_arg("sequencer_headers", |arg| arg.hide(true).long("__rollup-sequencer-headers-disabled"))
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
        let Self { execution_chain, execution, consensus } = self;
        let mut execution_chain = match execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let consensus_chain = resolved_chain.consensus_chain_args();
        let mut execution = execution;
        let mut consensus_config: ConsensusNodeConfigArgs = consensus.into();
        Self::derive_execution_upgrade_signal_l1_rpc(&mut execution, &consensus_config);
        consensus_config.upgrade_signal = execution.standard.rollup_args.upgrade_signal.clone();
        let consensus_args = ConsensusNodeArgs::new(consensus_chain, consensus_config);
        let mut rollup_config = consensus_args.load_rollup_config()?;

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            if let Some(signal_config) = execution.standard.rollup_args.upgrade_signal.config()?
                && signal_config.mode.applies_at_startup()
            {
                Self::apply_shared_startup_upgrade_signal(
                    &mut execution_chain,
                    &mut rollup_config,
                    &signal_config,
                    consensus_args.config.l1_rpc_args.l1_eth_rpc.clone(),
                )
                .await?;
            }

            let execution = execution
                .into_launch_config(execution_chain)
                .with_auth_ipc()
                .with_upgrade_signal_startup_already_applied();
            let l2_engine_rpc = engine_ipc_url(execution.auth_ipc_path())?;

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
            let consensus_exit = consensus_args
                .start_with_overrides_and_cancellation_and_upgrade_signal_startup(
                    rollup_config,
                    overrides,
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

    async fn apply_shared_startup_upgrade_signal(
        execution_chain: &mut Arc<BaseChainSpec>,
        rollup_config: &mut RollupConfig,
        signal_config: &UpgradeSignalConfig,
        l1_rpc: Url,
    ) -> eyre::Result<()> {
        let reader = AlloyUpgradeSignalReader::new(
            RootProvider::new_http(l1_rpc),
            signal_config.contract_address,
        );
        let schedule = match reader.read_schedule(&signal_config.hardfork_ids).await {
            Ok(schedule) => schedule,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors(&signal_config.hardfork_ids);
                return Err(error.into());
            }
        };

        UpgradeSignalMetrics::record_schedule(&schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = signal.activation_timestamp,
                minimum_protocol_version = %signal.protocol_version,
                node_protocol_version = %signal_config.node_protocol_version,
                l1_block_number = signal.l1_block_number,
                "read dynamic upgrade signal for integrated startup"
            );
        }

        let application_schedule = signal_config.application_schedule(&schedule);
        signal_config.validate_schedule_protocol_versions(&application_schedule)?;
        ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            Arc::make_mut(execution_chain),
            &application_schedule,
        )?;
        ConsensusNodeArgs::apply_schedule_to_rollup_config(rollup_config, &application_schedule);

        Ok(())
    }

    fn derive_execution_upgrade_signal_l1_rpc(
        execution: &mut ExecutionNodeArgs,
        consensus_config: &ConsensusNodeConfigArgs,
    ) {
        // The integrated command has one L1 RPC source of truth. Standalone execution nodes need
        // `--upgrade-signal.l1-rpc`, but `base rpc` derives it from the consensus L1 RPC.
        execution.standard.rollup_args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc =
            Some(consensus_config.l1_rpc_args.l1_eth_rpc.clone());
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
    use base_consensus_cli::ConsensusNodeConfigArgs;
    use clap::Parser;

    use super::RpcCommand;
    use crate::{cli::BaseCli, commands::BaseCommand, config::ChainArg};

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
    fn parses_upgrade_signal_args_once() {
        let cli = BaseCli::parse_from(rpc_args(&[
            "base",
            "rpc",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.hardfork-id",
            "azul",
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
        assert_eq!(rpc.execution.standard.rollup_args.upgrade_signal.hardfork_ids, ["azul"]);
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

        RpcCommand::derive_execution_upgrade_signal_l1_rpc(&mut rpc.execution, &consensus_config);

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
