//! Integration tests for RPC hook accumulation.
//!
//! This test verifies that multiple RPC-providing extensions are all accessible
//! when installed together, ensuring that the `BaseBuilder` properly accumulates
//! RPC hooks instead of replacing them.
//!
//! Related issues:
//! - Fixes verification for #438
//! - Discussed in #446

use std::sync::Arc;

use alloy_primitives::TxHash;
use base_client_node::{BaseNodeExtension, test_utils::load_chain_spec};
use base_metering::MeteringExtension;
use base_txpool::{Status, TransactionStatusResponse, TxPoolExtension, TxpoolConfig};
use eyre::Result;
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};

/// Test that multiple RPC-providing extensions are all accessible when installed together.
///
/// This verifies that:
/// 1. TxPool RPC (`base_transactionStatus`) is reachable
/// 2. Metering RPC (`base_meterBlockByNumber`) is reachable
///
/// Both should work simultaneously when both extensions are installed.
#[tokio::test(flavor = "multi_thread")]
async fn rpc_hooks_accumulate_across_extensions() -> Result<()> {
    // Initialize tracing for better test output
    base_client_node::test_utils::init_silenced_tracing();

    let chain_spec = load_chain_spec();

    // Install multiple RPC-providing extensions
    let mut extensions: Vec<Box<dyn BaseNodeExtension>> = Vec::new();
    extensions.push(Box::new(TxPoolExtension::new(TxpoolConfig {
        tracing_enabled: false,
        tracing_logs_enabled: false,
        sequencer_rpc: None,
        flashblocks_config: None,
    })));
    extensions.push(Box::new(MeteringExtension::new(true, None)));

    let node =
        base_client_node::test_utils::LocalNode::new(extensions, Arc::clone(&chain_spec)).await?;

    let client = HttpClientBuilder::default().build(format!("http://{}", node.http_addr()))?;

    // TxPool RPC should be reachable
    let status: TransactionStatusResponse =
        client.request("base_transactionStatus", rpc_params![TxHash::ZERO]).await?;
    assert_eq!(Status::Unknown, status.status);

    // Metering RPC should also be reachable
    // Note: This may return an error (e.g., block not found) but should NOT return MethodNotFound
    let meter_result = client
        .request::<serde_json::Value, _>("base_meterBlockByNumber", rpc_params!["latest"])
        .await;

    match meter_result {
        Ok(_) => {
            // Method exists and returned successfully
        }
        Err(jsonrpsee::core::ClientError::Call(err)) => {
            // Method exists but returned an error - this is fine as long as it's not MethodNotFound
            assert_ne!(
                err.code(),
                jsonrpsee::types::ErrorCode::MethodNotFound.code(),
                "metering RPC missing - RPC hooks are not accumulating properly"
            );
        }
        Err(err) => panic!("unexpected error from metering RPC: {err}"),
    }

    Ok(())
}
