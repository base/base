//! Verifies that sequencer RPC services are ready when node startup returns.

use alloy_primitives::B256;
use base_common_client_ethereum::Provider;
use base_execution_rpc::{Status, TransactionStatusResponse};
use base_node_service::BuilderConfig;
use base_testing_devnet::builder_test_utils::LocalInstanceBuilder;

#[tokio::test]
async fn built_in_transaction_status_is_ready_after_launch() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests()).build().await?;
    let provider = instance.provider().await?;
    let response: TransactionStatusResponse =
        provider.client().request("base_transactionStatus", (B256::ZERO,)).await?;
    assert_eq!(response.status, Status::Unknown);
    Ok(())
}
