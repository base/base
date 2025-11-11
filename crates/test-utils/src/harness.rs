//! Unified test harness combining node, engine API, and flashblocks functionality

use crate::accounts::TestAccounts;
use crate::engine::{EngineApi, IpcEngine};
use crate::node::{LocalFlashblocksState, LocalNode, LocalNodeProvider, OpAddOns, OpBuilder};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{bytes, Bytes, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::BlockNumberOrTag;
use alloy_rpc_types_engine::PayloadAttributes;
use base_reth_flashblocks_rpc::subscription::Flashblock;
use eyre::{eyre, Result};
use futures_util::Future;
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_optimism_node::OpNode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

const BLOCK_TIME_SECONDS: u64 = 2;
const GAS_LIMIT: u64 = 200_000_000;
const NODE_STARTUP_DELAY_MS: u64 = 500;
const BLOCK_BUILD_DELAY_MS: u64 = 100;
// Pre-captured L1 block info deposit transaction required by the Optimism EVM.
const L1_BLOCK_INFO_DEPOSIT_TX: Bytes = bytes!("0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000");

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

    pub fn blockchain_provider(&self) -> LocalNodeProvider {
        self.node.blockchain_provider()
    }

    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.node.flashblocks_state()
    }

    pub fn rpc_url(&self) -> String {
        format!("http://{}", self.node.http_api_addr)
    }

    pub async fn build_block_from_transactions(&self, mut transactions: Vec<Bytes>) -> Result<()> {
        // Ensure the block always starts with the required L1 block info deposit.
        if !transactions
            .first()
            .is_some_and(|tx| tx == &L1_BLOCK_INFO_DEPOSIT_TX)
        {
            transactions.insert(0, L1_BLOCK_INFO_DEPOSIT_TX.clone());
        }

        let latest_block = self
            .provider()
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("No genesis block found"))?;

        let parent_hash = latest_block.header.hash;
        let parent_beacon_block_root = latest_block
            .header
            .parent_beacon_block_root
            .unwrap_or(B256::ZERO);
        let next_timestamp = latest_block.header.timestamp + BLOCK_TIME_SECONDS;

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

    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.node.send_flashblock(flashblock).await
    }

    pub async fn send_flashblocks<I>(&self, flashblocks: I) -> Result<()>
    where
        I: IntoIterator<Item = Flashblock>,
    {
        for flashblock in flashblocks {
            self.send_flashblock(flashblock).await?;
        }
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
