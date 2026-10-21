//! Tests for custom genesis block number support.

use std::sync::Arc;

use alloy_consensus::BlockHeader;
use alloy_genesis::Genesis;
use alloy_primitives::B256;
use base_execution_chainspec::BaseChainSpecBuilder;
use base_node_core::{
    BaseNode,
    utils::{advance_chain, payload_attributes},
};
use reth_e2e_test_utils::E2ETestSetupBuilder;
use reth_provider::{HeaderProvider, StageCheckpointReader};
use reth_stages_types::StageId;
use reth_tasks::Runtime;
use tokio::sync::Mutex;

/// Tests that a Base node can initialize with a custom genesis block number.
#[tokio::test(flavor = "multi_thread")]
async fn test_base_node_custom_genesis_number() {
    reth_tracing::init_test_tracing();

    let genesis_number = 1000;

    // Create genesis with custom block number (1000)
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("../assets/genesis.json")).unwrap();
    genesis.number = Some(genesis_number);
    genesis.parent_hash = Some(B256::random());

    let chain_spec =
        Arc::new(BaseChainSpecBuilder::base_mainnet().genesis(genesis).ecotone_activated().build());

    let runtime = Runtime::test();
    let (mut node, wallet) = E2ETestSetupBuilder::<BaseNode>::new(1, chain_spec)
        .with_runtime(runtime.clone())
        .with_attributes_generator(payload_attributes)
        .build_single()
        .await
        .expect("Failed to launch node");

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

    let block_payloads = advance_chain(1, &mut node, Arc::new(Mutex::new(wallet))).await.unwrap();

    assert_eq!(block_payloads.len(), 1);
    let block = block_payloads.first().unwrap().block();

    // Verify the new block is at 1001 (genesis 1000 + 1)
    assert_eq!(
        block.number(),
        1001,
        "Block number should be 1001 after advancing from genesis 100"
    );

    // Stop the node's tasks before the process exits, otherwise they can still run during exit
    // teardown and abort the test binary.
    drop(node);
    runtime.graceful_shutdown();
}
