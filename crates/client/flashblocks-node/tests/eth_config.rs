#![allow(missing_docs)]

use std::sync::Arc;

use alloy_eips::eip7910::EthConfig;
use alloy_provider::Provider;
use base_execution_chainspec::OpChainSpec;
use base_node_runner::test_utils::{TestHarnessBuilder, build_test_genesis_v1};
use eyre::Result;

fn assert_zero_blob_schedule(config: &EthConfig) {
    let current = config.current.blob_schedule;
    assert_eq!(current.update_fraction, 0);
    assert_eq!(current.max_blob_count, 0);
    assert_eq!(current.target_blob_count, 0);

    if let Some(next) = config.next.as_ref() {
        assert_eq!(next.blob_schedule.update_fraction, 0);
        assert_eq!(next.blob_schedule.max_blob_count, 0);
        assert_eq!(next.blob_schedule.target_blob_count, 0);
    }

    if let Some(last) = config.last.as_ref() {
        assert_eq!(last.blob_schedule.update_fraction, 0);
        assert_eq!(last.blob_schedule.max_blob_count, 0);
        assert_eq!(last.blob_schedule.target_blob_count, 0);
    }
}

#[tokio::test]
async fn eth_config_available_on_base_v1_node() -> Result<()> {
    let harness = TestHarnessBuilder::new()
        .with_chain_spec(Arc::new(OpChainSpec::from_genesis(build_test_genesis_v1())))
        .build()
        .await?;
    let provider = harness.provider();

    let config = provider.client().request_noparams::<EthConfig>("eth_config").await?;
    assert_zero_blob_schedule(&config);

    Ok(())
}
