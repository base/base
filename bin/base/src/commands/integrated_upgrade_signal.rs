//! Shared upgrade-signal helpers for integrated Base commands.

use std::sync::Arc;

use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_consensus_cli::ConsensusNodeArgs;
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::ExecutionUpgradeSignal;
use base_upgrade_signal::{
    UpgradeSignalConfig, UpgradeSignalL1RpcArgs, UpgradeSignalMetricLayer,
    UpgradeSignalRuntimeValidation,
};
use url::Url;

/// Returns the runtime validation context shared by integrated commands.
pub(super) const fn runtime_validation_for_execution_chain(
    execution_chain: &BaseChainSpec,
) -> UpgradeSignalRuntimeValidation {
    UpgradeSignalRuntimeValidation::with_activation_admin_address(
        execution_chain.activation_admin_address,
    )
}

/// Applies the startup upgrade signal once to both execution and consensus configs.
pub(super) async fn apply_startup_signal(
    execution_chain: &mut Arc<BaseChainSpec>,
    rollup_config: &mut RollupConfig,
    signal_config: &UpgradeSignalConfig,
    l1_rpc: Url,
    log_context: &'static str,
) -> eyre::Result<()> {
    let reader = signal_config.reader(RootProvider::new_http(l1_rpc));
    let application_schedule = signal_config
        .read_validated_application_schedule(
            &reader,
            log_context,
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

/// Returns the integrated execution upgrade-signal L1 RPC or a checked internal-error message.
pub(super) fn required_execution_l1_rpc(
    upgrade_signal_l1_rpc: &UpgradeSignalL1RpcArgs,
) -> eyre::Result<Url> {
    upgrade_signal_l1_rpc
        .upgrade_signal_l1_rpc
        .clone()
        .ok_or_else(|| eyre::eyre!("upgrade signal L1 RPC not derived; this is a bug"))
}
