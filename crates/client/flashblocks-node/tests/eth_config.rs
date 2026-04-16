//! Integration tests for the `eth_config` RPC endpoint.

use std::sync::Arc;

use alloy_eips::eip7910::{EthConfig, EthForkConfig, SystemContract};
use alloy_provider::Provider;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarnessBuilder;
use base_test_utils::build_test_genesis_azul;
use eyre::Result;

fn assert_zero_blob_schedule(config: &EthConfig) {
    let current = config.current.blob_schedule;
    assert_eq!(current.update_fraction, 0);
    assert_eq!(current.max_blob_count, 0);
    assert_eq!(current.target_blob_count, 0);
    // `min_blob_fee` is omitted from the EIP-7840 wire format, so deserialization falls back to
    // the protocol default of `1` even though Base zeroes the advertised blob capacity fields.
    assert_eq!(current.min_blob_fee, 1);
    assert_eq!(current.max_blobs_per_tx, 0);
    assert_eq!(current.blob_base_cost, 0);

    if let Some(next) = config.next.as_ref() {
        assert_eq!(next.blob_schedule.update_fraction, 0);
        assert_eq!(next.blob_schedule.max_blob_count, 0);
        assert_eq!(next.blob_schedule.target_blob_count, 0);
        assert_eq!(next.blob_schedule.min_blob_fee, 1);
        assert_eq!(next.blob_schedule.max_blobs_per_tx, 0);
        assert_eq!(next.blob_schedule.blob_base_cost, 0);
    }

    if let Some(last) = config.last.as_ref() {
        assert_eq!(last.blob_schedule.update_fraction, 0);
        assert_eq!(last.blob_schedule.max_blob_count, 0);
        assert_eq!(last.blob_schedule.target_blob_count, 0);
        assert_eq!(last.blob_schedule.min_blob_fee, 1);
        assert_eq!(last.blob_schedule.max_blobs_per_tx, 0);
        assert_eq!(last.blob_schedule.blob_base_cost, 0);
    }
}

fn assert_supported_system_contracts(fork: &EthForkConfig) {
    assert!(
        fork.system_contracts.contains_key(&SystemContract::BeaconRoots),
        "expected BeaconRoots to remain enabled at activation_time={}",
        fork.activation_time
    );
    assert!(
        !fork.system_contracts.contains_key(&SystemContract::DepositContract),
        "DepositContract should be filtered at activation_time={}",
        fork.activation_time
    );
    assert!(
        !fork.system_contracts.contains_key(&SystemContract::ConsolidationRequestPredeploy),
        "ConsolidationRequestPredeploy should be filtered at activation_time={}",
        fork.activation_time
    );
    assert!(
        !fork.system_contracts.contains_key(&SystemContract::WithdrawalRequestPredeploy),
        "WithdrawalRequestPredeploy should be filtered at activation_time={}",
        fork.activation_time
    );
    assert!(
        fork.system_contracts.keys().all(|contract| matches!(
            contract,
            SystemContract::BeaconRoots | SystemContract::HistoryStorage
        )),
        "unexpected system contracts at activation_time={}: {:?}",
        fork.activation_time,
        fork.system_contracts.keys().collect::<Vec<_>>()
    );
}

fn assert_sanitized_system_contracts(config: &EthConfig) {
    assert!(
        config.current.system_contracts.contains_key(&SystemContract::HistoryStorage),
        "expected HistoryStorage to remain enabled in the active Azul config"
    );
    assert_supported_system_contracts(&config.current);

    if let Some(next) = config.next.as_ref() {
        assert_supported_system_contracts(next);
    }

    if let Some(last) = config.last.as_ref() {
        assert_supported_system_contracts(last);
    }
}

#[tokio::test]
async fn eth_config_available_on_base_azul_node() -> Result<()> {
    let harness = TestHarnessBuilder::new()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(build_test_genesis_azul())))
        .build()
        .await?;
    let provider = harness.provider();

    let config = provider.client().request_noparams::<EthConfig>("eth_config").await?;
    assert_zero_blob_schedule(&config);
    assert_sanitized_system_contracts(&config);

    Ok(())
}
