//! Unified test harness combining node and engine helpers, plus optional flashblocks adapter.

use std::time::Duration;

use alloy_eips::{BlockHashOrNumber, eip7685::Requests};
use alloy_primitives::{B64, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use base_client_primitives::BaseNodeExtension;
use eyre::{Result, eyre};
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth::providers::{BlockNumReader, BlockReader, ChainSpecProvider};
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::{Block as BlockT, RecoveredBlock};
use tokio::time::sleep;

use crate::{
    BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, GAS_LIMIT, L1_BLOCK_INFO_DEPOSIT_TX,
    NODE_STARTUP_DELAY_MS,
    engine::{EngineApi, IpcEngine},
    node::{LocalNode, LocalNodeProvider},
    tracing::init_silenced_tracing,
};

/// Builder for configuring and launching a test harness.
#[derive(Debug, Default)]
pub struct TestHarnessBuilder {
    extensions: Vec<Box<dyn BaseNodeExtension>>,
}

impl TestHarnessBuilder {
    /// Create a new builder with no extensions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an extension to be applied during node launch.
    pub fn with_extension(mut self, ext: impl BaseNodeExtension + 'static) -> Self {
        self.extensions.push(Box::new(ext));
        self
    }

    /// Build and launch the test harness.
    pub async fn build(self) -> Result<TestHarness> {
        init_silenced_tracing();
        let node = LocalNode::new(self.extensions).await?;
        let engine = node.engine_api()?;

        sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

        Ok(TestHarness { node, engine })
    }
}

/// High-level façade that bundles a local node, engine API client, and common helpers.
#[derive(Debug)]
pub struct TestHarness {
    node: LocalNode,
    engine: EngineApi<IpcEngine>,
}

impl TestHarness {
    /// Launch a new harness using the default configuration (no extensions).
    pub async fn new() -> Result<Self> {
        TestHarnessBuilder::new().build().await
    }

    /// Create a builder for configuring the test harness with extensions.
    pub fn builder() -> TestHarnessBuilder {
        TestHarnessBuilder::new()
    }

    /// Create a harness from pre-built parts.
    ///
    /// This is useful when you need to capture extension state before building the harness.
    pub(crate) fn from_parts(node: LocalNode, engine: EngineApi<IpcEngine>) -> Self {
        Self { node, engine }
    }

    /// Return an Optimism JSON-RPC provider connected to the harness node.
    pub fn provider(&self) -> RootProvider<Optimism> {
        self.node.provider().expect("provider should always be available after node initialization")
    }

    /// Access the low-level blockchain provider for direct database queries.
    pub fn blockchain_provider(&self) -> LocalNodeProvider {
        self.node.blockchain_provider()
    }

    /// HTTP URL for sending JSON-RPC requests to the local node.
    pub fn rpc_url(&self) -> String {
        format!("http://{}", self.node.http_api_addr)
    }

    /// Websocket URL for subscribing to JSON-RPC notifications.
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.node.ws_api_addr)
    }

    /// Return a JSON-RPC client connected to the harness node.
    pub fn rpc_client(&self) -> Result<RpcClient> {
        let url = self.rpc_url().parse()?;
        Ok(RpcClient::new_http(url))
    }

    /// Build a block using the provided transactions and push it through the engine.
    pub async fn build_block_from_transactions(&self, mut transactions: Vec<Bytes>) -> Result<()> {
        // Ensure the block always starts with the required L1 block info deposit.
        if transactions.first().is_none_or(|tx| tx != &L1_BLOCK_INFO_DEPOSIT_TX) {
            transactions.insert(0, L1_BLOCK_INFO_DEPOSIT_TX.clone());
        }

        let latest_block = self
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("No genesis block found"))?;

        let parent_hash = latest_block.header.hash;
        let parent_beacon_block_root =
            latest_block.header.parent_beacon_block_root.unwrap_or(B256::ZERO);
        let next_timestamp = latest_block.header.timestamp + BLOCK_TIME_SECONDS;

        let min_base_fee = latest_block.header.base_fee_per_gas.unwrap_or_default();
        let chain_spec = self.node.blockchain_provider().chain_spec();
        let base_fee_params = chain_spec.base_fee_params_at_timestamp(next_timestamp);
        let eip_1559_params = ((base_fee_params.max_change_denominator as u64) << 32)
            | (base_fee_params.elasticity_multiplier as u64);

        let payload_attributes = OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_timestamp,
                parent_beacon_block_root: Some(parent_beacon_block_root),
                withdrawals: Some(vec![]),
                ..Default::default()
            },
            transactions: Some(transactions),
            gas_limit: Some(GAS_LIMIT),
            no_tx_pool: Some(true),
            min_base_fee: Some(min_base_fee),
            eip_1559_params: Some(B64::from(eip_1559_params)),
        };

        let forkchoice_result = self
            .engine
            .update_forkchoice(parent_hash, parent_hash, Some(payload_attributes))
            .await?;

        let payload_id = forkchoice_result
            .payload_id
            .ok_or_else(|| eyre!("Forkchoice update did not return payload ID"))?;

        sleep(Duration::from_millis(BLOCK_BUILD_DELAY_MS)).await;

        let payload_envelope = self.engine.get_payload(payload_id).await?;

        let execution_requests = if payload_envelope.execution_requests.is_empty() {
            Requests::default()
        } else {
            Requests::new(payload_envelope.execution_requests)
        };

        let payload_status = self
            .engine
            .new_payload(
                payload_envelope.execution_payload,
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

        self.engine.update_forkchoice(parent_hash, new_block_hash, None).await?;

        Ok(())
    }

    /// Advance the canonical chain by `n` empty blocks.
    pub async fn advance_chain(&self, n: u64) -> Result<()> {
        for _ in 0..n {
            self.build_block_from_transactions(vec![]).await?;
        }
        Ok(())
    }

    /// Return the latest recovered block as seen by the local blockchain provider.
    pub fn latest_block(&self) -> RecoveredBlock<OpBlock> {
        let provider = self.blockchain_provider();
        let best_number = provider.best_block_number().expect("able to read best block number");
        let block = provider
            .block(BlockHashOrNumber::Number(best_number))
            .expect("able to load canonical block")
            .expect("canonical block exists");
        BlockT::try_into_recovered(block).expect("able to recover canonical block")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use alloy_provider::Provider;

    use super::*;
    use crate::Account;

    #[tokio::test]
    async fn test_harness_setup() -> Result<()> {
        let harness = TestHarness::new().await?;

        let provider = harness.provider();
        let chain_id = provider.get_chain_id().await?;
        assert_eq!(chain_id, crate::BASE_CHAIN_ID);

        let alice_balance = provider.get_balance(Account::Alice.address()).await?;
        assert!(alice_balance > U256::ZERO);

        let block_number = provider.get_block_number().await?;
        harness.advance_chain(5).await?;
        let new_block_number = provider.get_block_number().await?;
        assert_eq!(new_block_number, block_number + 5);

        Ok(())
    }
}
