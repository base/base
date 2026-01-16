//! Block driver for the devnet.
//!
//! Drives block production via Engine API, similar to what op-node/base-consensus does,
//! but simplified for local testing without requiring an L1 connection.

use std::time::Duration;

use alloy_eips::eip7685::Requests;
use alloy_primitives::{B64, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use base_client_node::test_utils::{
    BLOCK_BUILD_DELAY_MS, EngineApi, GAS_LIMIT, HttpEngine, L1_BLOCK_INFO_DEPOSIT_TX,
};
use eyre::{Result, eyre};
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Configuration for the block driver.
#[derive(Debug, Clone)]
pub(crate) struct BlockDriverConfig {
    /// Engine API URL for the client node.
    pub engine_url: String,
    /// HTTP RPC URL for the client node (to query latest block).
    pub rpc_url: String,
    /// Block time in milliseconds.
    pub block_time_ms: u64,
}

/// Runs the block driver loop, producing blocks at the configured interval.
pub(crate) async fn run_block_driver(config: BlockDriverConfig) -> Result<()> {
    info!("Starting block driver with {}ms block time", config.block_time_ms);

    // Wait for nodes to be ready
    sleep(Duration::from_secs(3)).await;

    // Create Engine API client
    let engine = EngineApi::<HttpEngine>::new(config.engine_url.clone())?;

    // Create RPC provider for querying latest block
    let rpc_url: url::Url = config.rpc_url.parse()?;
    let provider = RootProvider::<Optimism>::new_http(rpc_url);

    // Wait for the node to be responsive
    let mut retries = 0;
    loop {
        match provider.get_chain_id().await {
            Ok(chain_id) => {
                info!("Block driver connected to chain {}", chain_id);
                break;
            }
            Err(e) => {
                retries += 1;
                if retries > 30 {
                    return Err(eyre!("Failed to connect to node after 30 retries: {}", e));
                }
                debug!("Waiting for node to be ready... (attempt {})", retries);
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    // Main block production loop
    let block_interval = Duration::from_millis(config.block_time_ms);

    loop {
        match produce_block(&engine, &provider).await {
            Ok(block_number) => {
                debug!("Produced block {}", block_number);
            }
            Err(e) => {
                warn!("Failed to produce block: {}", e);
            }
        }

        sleep(block_interval).await;
    }
}

/// Produces a single block using the Engine API.
async fn produce_block(
    engine: &EngineApi<HttpEngine>,
    provider: &RootProvider<Optimism>,
) -> Result<u64> {
    // Get latest block
    let latest_block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre!("No latest block found"))?;

    let parent_hash = latest_block.header.hash;
    let parent_beacon_block_root =
        latest_block.header.parent_beacon_block_root.unwrap_or(B256::ZERO);
    let next_timestamp = latest_block.header.timestamp + 2; // 2 second block time

    // Build payload attributes with L1 deposit tx
    let transactions: Vec<Bytes> = vec![L1_BLOCK_INFO_DEPOSIT_TX];

    // Get base fee from parent block
    let min_base_fee = latest_block.header.base_fee_per_gas.unwrap_or(1);

    // EIP-1559 params: elasticity=6, denominator=50 (matching our genesis)
    // Format: (denominator << 32) | elasticity
    let eip_1559_params: u64 = (50 << 32) | 6;

    let payload_attributes = OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: next_timestamp,
            parent_beacon_block_root: Some(parent_beacon_block_root),
            withdrawals: Some(vec![]),
            ..Default::default()
        },
        transactions: Some(transactions),
        gas_limit: Some(GAS_LIMIT),
        no_tx_pool: Some(false), // Include txpool transactions
        min_base_fee: Some(min_base_fee),
        eip_1559_params: Some(B64::from(eip_1559_params)),
    };

    // Start building block
    let forkchoice_result =
        engine.update_forkchoice(parent_hash, parent_hash, Some(payload_attributes)).await?;

    let payload_id = forkchoice_result
        .payload_id
        .ok_or_else(|| eyre!("Forkchoice update did not return payload ID"))?;

    // Wait for block to be built
    sleep(Duration::from_millis(BLOCK_BUILD_DELAY_MS)).await;

    // Get the built payload
    let payload_envelope = engine.get_payload(payload_id).await?;

    // Submit the payload
    let execution_requests = if payload_envelope.execution_requests.is_empty() {
        Requests::default()
    } else {
        Requests::new(payload_envelope.execution_requests)
    };

    let payload_status = engine
        .new_payload(
            payload_envelope.execution_payload.clone(),
            vec![],
            payload_envelope.parent_beacon_block_root,
            execution_requests,
        )
        .await?;

    if payload_status.status.is_invalid() {
        return Err(eyre!("Engine rejected payload: {:?}", payload_status));
    }

    let new_block_hash = payload_status
        .latest_valid_hash
        .ok_or_else(|| eyre!("Payload status missing latest_valid_hash"))?;

    // Finalize the block
    engine.update_forkchoice(new_block_hash, new_block_hash, None).await?;

    // Return the new block number (parent + 1)
    let new_block_number = latest_block.header.number + 1;

    Ok(new_block_number)
}
