//! Base node setup and chain advancement for integration tests.

use std::sync::Arc;

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use base_execution_chainspec::BaseChainSpecBuilder;
use base_execution_payload_builder::{
    BaseBuiltPayload, BasePayloadBuilderAttributes, payload::EthPayloadBuilderAttributes,
};
use base_node_core::BaseNode;
use tokio::sync::Mutex;

use crate::{NodeHelperType, transaction::TransactionTestContext, wallet::Wallet};

/// Base Node Helper type
pub type BaseTestNode = NodeHelperType;

/// Base node integration-test helpers.
#[derive(Debug)]
pub struct BaseNodeTestUtils;

impl BaseNodeTestUtils {
    /// Supplies Base components and add-ons for a temporary node.
    pub fn test_setup() -> base_node_core::BaseNode {
        let node = BaseNode::default();
        node
    }

    /// Returns the shared Base integration-test genesis.
    pub fn genesis() -> Genesis {
        serde_json::from_str(include_str!("../test_data/base-genesis.json"))
            .expect("valid test genesis")
    }

    /// Creates the initial setup with `num_nodes` of the node config, started and connected.
    pub async fn setup(num_nodes: usize) -> eyre::Result<(Vec<BaseTestNode>, Wallet)> {
        let genesis = Self::genesis();
        crate::setup_engine(
            Self::test_setup,
            num_nodes,
            Arc::new(
                BaseChainSpecBuilder::base_mainnet().genesis(genesis).ecotone_activated().build(),
            ),
            false,
            Default::default(),
            Self::payload_attributes,
        )
        .await
    }

    /// Advance the chain with sequential payloads returning them in the end.
    pub async fn advance_chain(
        length: usize,
        node: &mut BaseTestNode,
        wallet: Arc<Mutex<Wallet>>,
    ) -> eyre::Result<Vec<BaseBuiltPayload>> {
        node.advance(length as u64, |_| {
            let wallet = Arc::clone(&wallet);
            Box::pin(async move {
                let mut wallet = wallet.lock().await;
                let tx_fut = TransactionTestContext::optimism_l1_block_info_tx(
                    wallet.chain_id,
                    wallet.inner.clone(),
                    wallet.inner_nonce,
                );
                wallet.inner_nonce += 1;
                tx_fut.await
            })
        })
        .await
    }

    /// Helper function to create a new eth payload attributes
    pub fn payload_attributes(timestamp: u64) -> BasePayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        };

        BasePayloadBuilderAttributes {
            payload_attributes: EthPayloadBuilderAttributes {
                id: Default::default(),
                parent: B256::ZERO,
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                has_withdrawals: attributes.withdrawals.is_some(),
                withdrawals: attributes.withdrawals.unwrap_or_default().into(),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                slot_number: None,
            },
            transactions: vec![],
            no_tx_pool: false,
            gas_limit: Some(30_000_000),
            eip_1559_params: None,
            min_base_fee: None,
        }
    }
}
