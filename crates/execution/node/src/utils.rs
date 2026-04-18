use std::sync::Arc;

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use base_execution_chainspec::BaseChainSpecBuilder;
use base_execution_payload_builder::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_e2e_test_utils::{
    NodeHelperType, TmpDB, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_api::NodeTypesWithDBAdapter;
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_provider::providers::BlockchainProvider;
use tokio::sync::Mutex;

use crate::BaseNode as OtherOpNode;

/// Base Node Helper type
pub type BaseNode =
    NodeHelperType<OtherOpNode, BlockchainProvider<NodeTypesWithDBAdapter<OtherOpNode, TmpDB>>>;

/// Creates the initial setup with `num_nodes` of the node config, started and connected.
pub async fn setup(num_nodes: usize) -> eyre::Result<(Vec<BaseNode>, Wallet)> {
    let genesis: Genesis =
        serde_json::from_str(include_str!("../tests/assets/genesis.json")).unwrap();
    reth_e2e_test_utils::setup_engine(
        num_nodes,
        Arc::new(BaseChainSpecBuilder::base_mainnet().genesis(genesis).ecotone_activated().build()),
        false,
        Default::default(),
        payload_attributes,
    )
    .await
}

/// Advance the chain with sequential payloads returning them in the end.
pub async fn advance_chain(
    length: usize,
    node: &mut BaseNode,
    wallet: Arc<Mutex<Wallet>>,
) -> eyre::Result<Vec<OpBuiltPayload>> {
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
pub fn payload_attributes<T>(timestamp: u64) -> OpPayloadBuilderAttributes<T> {
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
    };

    OpPayloadBuilderAttributes {
        payload_attributes: EthPayloadBuilderAttributes::new(B256::ZERO, attributes),
        transactions: vec![],
        no_tx_pool: false,
        gas_limit: Some(30_000_000),
        eip_1559_params: None,
        min_base_fee: None,
    }
}
