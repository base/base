//! Action tests that exercise [`TestRollupNode`] end-to-end.
//!
//! [`TestRollupNode`] composes the real 8-stage derivation pipeline with
//! [`ActionEngineClient`] (real EVM execution via revm) and
//! [`TestGossipTransport`] (in-memory P2P channel). These tests drive
//! derivation through the node's own step loop and verify state-root
//! correctness implicitly via the shared `block_hash_registry`.
//!
//! [`TestRollupNode`]: base_action_harness::TestRollupNode
//! [`ActionEngineClient`]: base_action_harness::ActionEngineClient
//! [`TestGossipTransport`]: base_action_harness::TestGossipTransport

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// [`TestRollupNode`] derives L2 blocks produced by the sequencer from
/// calldata batches using real EVM execution.
///
/// 1. A sequencer builds 3 L2 blocks.
/// 2. All blocks are batched into one L1 block.
/// 3. The node is initialized and signalled about the new L1 head.
/// 4. `run_until_idle` derives all 3 blocks; `safe_head` advances to 3.
///
/// [`TestRollupNode`]: base_action_harness::TestRollupNode
#[tokio::test]
async fn test_rollup_node_derives_batched_blocks() {
    const L2_BLOCK_COUNT: u64 = 3;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(chain.clone());
    let transport = h.create_supervised_p2p(&mut sequencer);
    let mut node = h.create_test_rollup_node(&sequencer, chain.clone(), transport);

    let mut source = ActionL2Source::new();
    for _ in 0..L2_BLOCK_COUNT {
        source.push(sequencer.build_next_block_with_single_transaction());
    }

    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected {L2_BLOCK_COUNT} blocks derived");
    assert_eq!(
        node.l2_safe_number(),
        L2_BLOCK_COUNT,
        "safe head should be at block {L2_BLOCK_COUNT}"
    );
}

/// P2P gossip advances `unsafe_head` before batches land on L1; derivation
/// then catches `safe_head` up to `unsafe_head`.
///
/// 1. A sequencer builds 5 L2 blocks and broadcasts each via [`SupervisedP2P`].
/// 2. The node is initialized; gossip is drained, advancing `unsafe_head` to 5.
///    `safe_head` stays at genesis (0).
/// 3. All blocks are batched into one L1 block.
/// 4. The node is signalled about the new L1 head; `run_until_idle` derives
///    all 5 blocks and `safe_head` catches up to `unsafe_head`.
///
/// [`SupervisedP2P`]: base_action_harness::SupervisedP2P
#[tokio::test]
async fn test_rollup_node_gossip_then_derivation() {
    const L2_BLOCK_COUNT: u64 = 5;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(chain.clone());
    let transport = h.create_supervised_p2p(&mut sequencer);
    let mut node = h.create_test_rollup_node(&sequencer, chain.clone(), transport);

    // Build blocks and gossip each one before any L1 batching.
    let mut source = ActionL2Source::new();
    for _ in 0..L2_BLOCK_COUNT {
        let block = sequencer.build_next_block_with_single_transaction();
        sequencer.broadcast_unsafe_block(&block);
        source.push(block);
    }

    // Initialize then step once to drain the gossip channel; unsafe_head should be at 5.
    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(
        node.l2_unsafe_number(),
        L2_BLOCK_COUNT,
        "unsafe_head should be at {L2_BLOCK_COUNT} after gossip drain"
    );
    assert_eq!(node.l2_safe_number(), 0, "safe_head must stay at genesis before batches land");

    // Batch and mine so the pipeline can derive.
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());
    let derived = node.run_until_idle().await;

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected {L2_BLOCK_COUNT} blocks derived");
    assert_eq!(node.l2_safe_number(), L2_BLOCK_COUNT, "safe_head should have caught up");
    assert_eq!(
        node.l2_unsafe_number(),
        node.l2_safe_number(),
        "unsafe_head and safe_head should be equal after safe chain caught up"
    );
}

/// Out-of-order gossip blocks are silently dropped; only sequential blocks
/// advance `unsafe_head`.
///
/// Block 3 arrives before blocks 1 and 2 — the gap guard drops it.
/// Block 1 arrives next and advances `unsafe_head` to 1.
/// Block 3 arrives again — still a gap (next expected is 2) — dropped again.
#[tokio::test]
async fn test_rollup_node_out_of_order_gossip_dropped() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(chain.clone());
    let transport = h.create_supervised_p2p(&mut sequencer);
    let mut node = h.create_test_rollup_node(&sequencer, chain, transport);

    let block1 = sequencer.build_next_block_with_single_transaction();
    let _block2 = sequencer.build_next_block_with_single_transaction();
    let block3 = sequencer.build_next_block_with_single_transaction();

    // Inject block 3 first — gap-jump, must be dropped.
    sequencer.broadcast_unsafe_block(&block3);
    node.initialize().await;
    assert_eq!(node.l2_unsafe_number(), 0, "gap-jump block 3 must be dropped");

    // Inject block 1 — sequential, must advance.
    sequencer.broadcast_unsafe_block(&block1);
    // step() calls drain_gossip() internally.
    node.step().await;
    assert_eq!(node.l2_unsafe_number(), 1, "block 1 should advance unsafe_head to 1");

    // Inject block 3 again — still a gap (unsafe_head=1, next expected=2).
    sequencer.broadcast_unsafe_block(&block3);
    node.step().await;
    assert_eq!(node.l2_unsafe_number(), 1, "block 3 must be dropped when unsafe_head=1");
}
