//! System tests for the L1 upgrade signal read by the in-process consensus nodes.
//!
//! Each test deploys a mock `ProtocolVersions` contract to the stack's L1 and verifies how the
//! consensus nodes consume its schedule in the different upgrade signal modes. Tests that
//! observe the process-local [`RuntimeUpgradeRegistry`] use unique L2 chain IDs because the
//! registry is shared by every in-process node in this test binary.

use std::time::Duration;

use base_common_genesis::{BaseUpgrade, RollupConfig, RuntimeUpgradeRegistry, UpgradeActivation};
use base_consensus_rpc::RollupNodeApiClient;
use base_system_tests::{SystemTestStack, SystemTestStackBuilder, UpgradeSignalStackOptions};
use base_upgrade_signal::UpgradeSignalMode;
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::time::{sleep, timeout};

/// Initial Cobalt activation timestamp seeded into the mock contract (2100-01-01).
const COBALT_ACTIVATION_TIMESTAMP: u64 = 4_102_444_800;
/// Rescheduled Cobalt activation timestamp for live updates (2101-01-01).
const COBALT_RESCHEDULED_TIMESTAMP: u64 = 4_133_980_800;
/// Longest wait for a live L1 schedule change to be re-applied (poll interval is 12s).
const LIVE_APPLY_TIMEOUT: Duration = Duration::from_secs(90);
/// Interval between runtime registry checks.
const LIVE_APPLY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Reads the rollup config served by a consensus node's RPC.
async fn rollup_config_via_rpc(rpc_url: &url::Url) -> Result<RollupConfig> {
    let client = HttpClientBuilder::default()
        .build(rpc_url.as_str())
        .wrap_err("Failed to build consensus RPC client")?;
    RollupNodeApiClient::rollup_config(&client)
        .await
        .wrap_err("Failed to fetch rollup config via RPC")
}

/// Starts a stack with the upgrade signal enabled for Cobalt at the given mode and chain ID.
async fn start_upgrade_signal_system(
    l2_chain_id: u64,
    mode: UpgradeSignalMode,
) -> Result<SystemTestStack> {
    SystemTestStackBuilder::new()
        .with_l2_chain_id(l2_chain_id)
        .with_upgrade_signal(
            UpgradeSignalStackOptions::new(mode)
                .with_upgrade(BaseUpgrade::Cobalt, COBALT_ACTIVATION_TIMESTAMP),
        )
        .build()
        .await
}

/// Startup-apply mode reads the L1 schedule before startup and applies it to the rollup config
/// served by both consensus nodes.
#[tokio::test]
async fn test_upgrade_signal_startup_apply_sets_rollup_config() -> Result<()> {
    let system = start_upgrade_signal_system(84_538_461, UpgradeSignalMode::StartupApply).await?;

    let builder_config =
        rollup_config_via_rpc(&system.l2_stack().builder_consensus_rpc_url()).await?;
    assert_eq!(
        builder_config.contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt),
        Some(COBALT_ACTIVATION_TIMESTAMP),
        "builder consensus should serve the L1-scheduled Cobalt activation"
    );

    let client_config =
        rollup_config_via_rpc(&system.l2_stack().client_consensus_rpc_url()).await?;
    assert_eq!(
        client_config.contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt),
        Some(COBALT_ACTIVATION_TIMESTAMP),
        "client consensus should serve the L1-scheduled Cobalt activation"
    );

    Ok(())
}

/// Metrics-only mode observes the L1 schedule without mutating the local rollup config or the
/// process-local runtime registry.
#[tokio::test]
async fn test_upgrade_signal_metrics_only_does_not_mutate_schedule() -> Result<()> {
    let l2_chain_id = 84_538_462;
    let system = start_upgrade_signal_system(l2_chain_id, UpgradeSignalMode::MetricsOnly).await?;

    let builder_config =
        rollup_config_via_rpc(&system.l2_stack().builder_consensus_rpc_url()).await?;
    assert_ne!(
        builder_config.contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt),
        Some(COBALT_ACTIVATION_TIMESTAMP),
        "metrics-only mode must not apply the L1 schedule to the rollup config"
    );
    assert_eq!(
        RuntimeUpgradeRegistry::activation(l2_chain_id, BaseUpgrade::Cobalt),
        None,
        "metrics-only mode must not write runtime activation overrides"
    );

    Ok(())
}

/// Runtime-admin mode applies the L1 schedule at startup and automatically re-applies observed
/// live L1 schedule changes to the process-local runtime registry.
#[tokio::test]
async fn test_upgrade_signal_runtime_admin_reapplies_live_schedule_change() -> Result<()> {
    let l2_chain_id = 84_538_463;
    let system = start_upgrade_signal_system(l2_chain_id, UpgradeSignalMode::RuntimeAdmin).await?;

    let builder_config =
        rollup_config_via_rpc(&system.l2_stack().builder_consensus_rpc_url()).await?;
    assert_eq!(
        builder_config.contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt),
        Some(COBALT_ACTIVATION_TIMESTAMP),
        "runtime-admin mode should apply the L1 schedule at startup"
    );

    let contract =
        system.upgrade_signal().expect("stack was built with the upgrade signal enabled");
    contract
        .set_schedule(&[(BaseUpgrade::Cobalt, COBALT_RESCHEDULED_TIMESTAMP)])
        .await
        .wrap_err("Failed to reschedule Cobalt on L1")?;

    timeout(LIVE_APPLY_TIMEOUT, async {
        loop {
            if RuntimeUpgradeRegistry::activation(l2_chain_id, BaseUpgrade::Cobalt)
                == Some(UpgradeActivation::Timestamp(COBALT_RESCHEDULED_TIMESTAMP))
            {
                return;
            }
            sleep(LIVE_APPLY_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| {
        eyre::eyre!(
            "live L1 reschedule was not re-applied to the runtime registry within {}s",
            LIVE_APPLY_TIMEOUT.as_secs()
        )
    })?;

    Ok(())
}
