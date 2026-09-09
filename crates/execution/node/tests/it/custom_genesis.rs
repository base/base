//! Tests for custom genesis block number support.

use std::sync::Arc;

use alloy_genesis::Genesis;
use alloy_primitives::B256;
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_consensus::BlockHeader;
use base_common_rpc_types_engine::ForkchoiceState;
use base_execution_chainspec::BaseChainSpecBuilder;
use base_node_core::NodeConfig;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::{
    BaseNodeTestUtils, node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_core::args::DatadirArgs;
use reth_provider::{BlockReaderIdExt, HeaderProvider, StageCheckpointReader};
use reth_stages_types::StageId;
use tokio::sync::Mutex;

/// Tests that a Base node can initialize with a custom genesis block number.
#[tokio::test(flavor = "multi_thread")]
async fn test_base_node_custom_genesis_number() {
    reth_tracing::init_test_tracing();

    let genesis_number = 1000;

    // Create genesis with custom block number (1000)
    let mut genesis: Genesis = BaseNodeTestUtils::genesis();
    genesis.number = Some(genesis_number);
    genesis.parent_hash = Some(B256::random());

    let chain_spec =
        Arc::new(BaseChainSpecBuilder::base_mainnet().genesis(genesis).ecotone_activated().build());

    let wallet = Arc::new(Mutex::new(Wallet::default().with_chain_id(chain_spec.chain().into())));

    // Configure and launch the node
    let config = NodeConfig::new(Arc::clone(&chain_spec)).with_unused_ports().with_datadir_args(
        DatadirArgs { datadir: reth_db::test_utils::tempdir_path().into(), ..Default::default() },
    );
    let db = create_test_rw_db_with_path(
        config
            .datadir
            .datadir
            .unwrap_or_chain_default(config.chain.chain(), config.datadir.clone())
            .db(),
    );
    let runtime = reth_tasks::Runtime::test();
    let node_handle = base_node_core::NodeLaunch::new(config.clone(), db, runtime.clone())
        .launch()
        .await
        .expect("Failed to launch node");

    let mut node = NodeTestContext::new(node_handle.node, BaseNodeTestUtils::payload_attributes)
        .await
        .unwrap();

    let genesis_hash = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(BlockNumberOrTag::Number(genesis_number))
        .unwrap()
        .expect("genesis header should exist")
        .hash();
    node.inner
        .execution
        .driver
        .fork_choice_updated(ForkchoiceState::same_hash(genesis_hash), None)
        .await
        .expect("able to seed forkchoice for custom genesis");

    // Verify stage checkpoints are initialized to genesis block number (1000)
    for stage in StageId::ALL {
        let checkpoint = node.inner.provider.get_stage_checkpoint(stage).unwrap();
        assert!(checkpoint.is_some(), "Stage {stage:?} checkpoint should exist");
        assert_eq!(
            checkpoint.unwrap().block_number,
            1000,
            "Stage {stage:?} checkpoint should be at genesis block 1000"
        );
    }

    // Query genesis block should succeed
    let genesis_header = node.inner.provider.header_by_number(genesis_number).unwrap();
    assert!(genesis_header.is_some(), "Genesis block at {genesis_number} should exist");

    // Query blocks before genesis should return None
    for block_num in [0, 1, genesis_number - 1] {
        let header = node.inner.provider.header_by_number(block_num).unwrap();
        assert!(header.is_none(), "Block {block_num} before genesis should not exist");
    }

    // Advance the chain with a single block
    let _ = wallet; // wallet available for future use
    let block_payloads = node
        .advance(1, |_| {
            Box::pin({
                let value = Arc::clone(&wallet);
                async move {
                    let mut wallet = value.lock().await;
                    let tx_fut = TransactionTestContext::optimism_l1_block_info_tx(
                        wallet.chain_id,
                        wallet.inner.clone(),
                        wallet.inner_nonce,
                    );
                    wallet.inner_nonce += 1;

                    tx_fut.await
                }
            })
        })
        .await
        .unwrap();

    assert_eq!(block_payloads.len(), 1);
    let block = block_payloads.first().unwrap().block();

    // Verify the new block is at 1001 (genesis 1000 + 1)
    assert_eq!(
        block.number(),
        1001,
        "Block number should be 1001 after advancing from genesis 100"
    );
}
