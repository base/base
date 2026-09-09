//! Example tests using the test suite framework.

#[path = "../fixtures/mod.rs"]
pub mod fixtures;
use std::sync::Arc;

use alloy_primitives::B256;
use base_common_rpc_types_engine::PayloadAttributes;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use eyre::Result;
use fixtures::BaseTestPayload;
use reth_e2e_test_utils::{
    BaseNodeTestUtils, E2ETestSetupBuilder,
    testsuite::{
        TestBuilder,
        actions::{
            BlockReference, CaptureBlock, CaptureBlockOnNode, CompareNodeChainTips, CreateFork,
            FinalizeBlock, MakeCanonical, ProduceBlocks, ReorgTo, SelectActiveNode,
        },
        setup::{NetworkSetup, Setup},
    },
};
use reth_engine_primitives::TreeConfig;

#[tokio::test]
async fn test_testsuite_produce_blocks() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_payload_attributes_converter(BaseTestPayload::attributes)
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        ))
        .with_network(NetworkSetup::single_node());

    let test = TestBuilder::new()
        .with_setup(setup)
        .with_action(ProduceBlocks::new(5))
        .with_action(MakeCanonical::new());

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

#[tokio::test]
async fn test_testsuite_create_fork() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_payload_attributes_converter(BaseTestPayload::attributes)
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        ))
        .with_network(NetworkSetup::single_node());

    let test = TestBuilder::new()
        .with_setup(setup)
        .with_action(ProduceBlocks::new(2))
        .with_action(MakeCanonical::new())
        .with_action(CreateFork::new(1, 3));

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

#[tokio::test]
async fn test_testsuite_reorg_with_tagging() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_payload_attributes_converter(BaseTestPayload::attributes)
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        ))
        .with_network(NetworkSetup::single_node());

    let test = TestBuilder::new()
        .with_setup(setup)
        .with_action(ProduceBlocks::new(1)) // produce block 1
        .with_action(CaptureBlock::new("fork_base"))
        .with_action(ProduceBlocks::new(2)) // produce blocks 2, 3
        .with_action(CaptureBlock::new("main_tip"))
        .with_action(MakeCanonical::new()) // make main chain tip canonical
        // block production finalizes the produced blocks, so re-establish finality at the fork
        // base: building below the finalized block is rejected as a too deep reorg
        .with_action(
            FinalizeBlock::new(BlockReference::Tag("fork_base".to_string()))
                .with_head(BlockReference::Tag("main_tip".to_string())),
        )
        // fork from block 1, produce blocks 2', 3'
        .with_action(CreateFork::new_from_tag("fork_base", 2))
        .with_action(CaptureBlock::new("fork_tip")) // tag fork tip
        .with_action(ReorgTo::new_from_tag("fork_tip")); // reorg to fork tip

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

#[tokio::test]
async fn test_testsuite_deep_reorg() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_payload_attributes_converter(BaseTestPayload::attributes)
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        ))
        .with_network(NetworkSetup::single_node())
        .with_tree_config(TreeConfig::default().with_state_root_fallback(true));

    let test = TestBuilder::new()
        .with_setup(setup)
        // receive newPayload and forkchoiceUpdated with block height 1
        .with_action(ProduceBlocks::new(1))
        .with_action(MakeCanonical::new())
        .with_action(CaptureBlock::new("block1"))
        // receive forkchoiceUpdated with block hash A as head (block A at height 2)
        .with_action(CreateFork::new(1, 1))
        .with_action(CaptureBlock::new("blockA_height2"))
        .with_action(MakeCanonical::new())
        // receive newPayload with block hash B and height 2
        .with_action(ReorgTo::new_from_tag("block1"))
        .with_action(CreateFork::new(1, 1))
        .with_action(CaptureBlock::new("blockB_height2"))
        // receive forkchoiceUpdated with block hash B as head
        .with_action(ReorgTo::new_from_tag("blockB_height2"));

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

/// Multi-node test demonstrating block creation and coordination across multiple nodes.
///
/// This test demonstrates the working multi-node framework:
/// - Multiple nodes start from the same genesis
/// - Nodes can be selected for specific operations
/// - Block production can happen on different nodes
/// - Chain tips can be compared between nodes
/// - Node-specific state is properly tracked
#[tokio::test]
async fn test_testsuite_multinode_block_production() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_payload_attributes_converter(BaseTestPayload::attributes)
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        ))
        .with_network(NetworkSetup::multi_node(2)) // Create 2 nodes
        .with_tree_config(TreeConfig::default().with_state_root_fallback(true));

    let test = TestBuilder::new()
        .with_setup(setup)
        // both nodes start from genesis
        .with_action(CaptureBlock::new("genesis"))
        .with_action(CompareNodeChainTips::expect_same(0, 1))
        // build main chain (blocks 1-3)
        .with_action(SelectActiveNode::new(0))
        .with_action(ProduceBlocks::new(3))
        .with_action(MakeCanonical::new())
        .with_action(CaptureBlockOnNode::new("node0_tip", 0))
        .with_action(CompareNodeChainTips::expect_same(0, 1))
        // node 0 already has the state and can continue producing blocks
        .with_action(ProduceBlocks::new(2))
        .with_action(MakeCanonical::new())
        .with_action(CaptureBlockOnNode::new("node0_tip_2", 0))
        // verify both nodes remain in sync
        .with_action(CompareNodeChainTips::expect_same(0, 1));

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

#[tokio::test]
async fn test_setup_builder_with_custom_tree_config() -> Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = Arc::new(
        BaseChainSpecBuilder::default()
            .chain(BaseChainSpec::mainnet().chain())
            .genesis(BaseNodeTestUtils::genesis())
            .ecotone_activated()
            .build(),
    );

    let (nodes, _wallet) = E2ETestSetupBuilder::new(1, chain_spec, |_| {
        BaseTestPayload::attributes(PayloadAttributes::default())
    })
    .with_tree_config_modifier(|config| {
        config.with_persistence_threshold(0).with_memory_block_buffer_target(5)
    })
    .build(BaseNodeTestUtils::test_setup)
    .await?;

    assert_eq!(nodes.len(), 1);

    let genesis_hash = nodes[0].block_hash(0);
    assert_ne!(genesis_hash, B256::ZERO);

    Ok(())
}
