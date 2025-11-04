//! Unified test harness combining node, engine API, and flashblocks functionality

use crate::accounts::TestAccounts;
use crate::engine::{EngineApi, IpcEngine};
use crate::node::{LocalNode, OpAddOns, OpBuilder};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{Bytes, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use eyre::{eyre, Result};
use futures_util::Future;
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_optimism_node::OpNode;
use std::time::Duration;
use tokio::time::sleep;

const BLOCK_TIME_SECONDS: u64 = 2;
const GAS_LIMIT: u64 = 200_000_000;
const NODE_STARTUP_DELAY_MS: u64 = 500;
const BLOCK_BUILD_DELAY_MS: u64 = 100;

pub struct TestHarness {
    node: LocalNode,
    engine: EngineApi<IpcEngine>,
    accounts: TestAccounts,
}

impl TestHarness {
    pub async fn new<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        let node = LocalNode::new(launcher).await?;
        let engine = node.engine_api()?;
        let accounts = TestAccounts::new();

        sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

        Ok(Self {
            node,
            engine,
            accounts,
        })
    }

    pub fn provider(&self) -> RootProvider<Optimism> {
        self.node
            .provider()
            .expect("provider should always be available after node initialization")
    }

    pub fn accounts(&self) -> &TestAccounts {
        &self.accounts
    }

    async fn build_block_from_transactions(&self, transactions: Vec<Bytes>) -> Result<()> {
        let latest_block = self
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("No genesis block found"))?;

        let parent_hash = latest_block.header.hash;
        let next_timestamp = latest_block.header.timestamp + BLOCK_TIME_SECONDS;

        let payload_attributes = OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_timestamp,
                parent_beacon_block_root: Some(B256::ZERO),
                withdrawals: Some(vec![]),
                ..Default::default()
            },
            transactions: Some(transactions),
            gas_limit: Some(GAS_LIMIT),
            no_tx_pool: Some(true),
            ..Default::default()
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

        self.engine
            .update_forkchoice(parent_hash, new_block_hash, None)
            .await?;

        Ok(())
    }

    pub async fn advance_chain(&self, n: u64) -> Result<()> {
        for _ in 0..n {
            self.build_block_from_transactions(vec![]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::node::default_launcher;

    use super::*;
    use alloy_primitives::U256;
    use alloy_provider::Provider;

    #[tokio::test]
    async fn test_harness_setup() -> Result<()> {
        reth_tracing::init_test_tracing();
        let harness = TestHarness::new(default_launcher).await?;

        assert_eq!(harness.accounts().alice.name, "Alice");
        assert_eq!(harness.accounts().bob.name, "Bob");

        let provider = harness.provider();
        let chain_id = provider.get_chain_id().await?;
        assert_eq!(chain_id, crate::node::BASE_CHAIN_ID);

        let alice_balance = provider
            .get_balance(harness.accounts().alice.address)
            .await?;
        assert!(alice_balance > U256::ZERO);

        let block_number = provider.get_block_number().await?;
        harness.advance_chain(5).await?;
        let new_block_number = provider.get_block_number().await?;
        assert_eq!(new_block_number, block_number + 5);

        Ok(())
    }
}
