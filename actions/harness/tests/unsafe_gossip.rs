//! Action tests for L2 unsafe-head gossip simulation.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Simulates the P2P gossip pattern:
///
/// 1. A sequencer builds 5 L2 blocks.
/// 2. Each block is gossiped to the verifier, advancing `unsafe_head` to 5.
///    Throughout this phase `safe_head` stays at genesis (0).
/// 3. All blocks are batched and submitted to L1; one L1 block is mined.
/// 4. The verifier is signalled about the new L1 head and runs full derivation.
///    `safe_head` catches up to `unsafe_head` (both at 5).
#[tokio::test]
async fn test_unsafe_chain_advances_safe_catches_up() {
    const L2_BLOCK_COUNT: u64 = 5;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // --- Phase 1: Build L2 blocks with the sequencer. ---
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut blocks = Vec::with_capacity(L2_BLOCK_COUNT as usize);
    for _ in 0..L2_BLOCK_COUNT {
        blocks.push(sequencer.build_next_block_with_single_transaction());
    }

    // Create the node before any mining so chain updates are visible.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // --- Phase 2 (setup): Batch + mine so the node can later derive. ---
    // All 5 blocks are encoded into one batcher transaction and mined into
    // a single L1 block before the node is used for derivation.
    let mut source = ActionL2Source::new();
    for block in &blocks {
        source.push(block.clone());
    }
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());
    let l1_block_1 = h.l1.tip_info();

    // Initialize: seed the genesis SystemConfig and drain the empty genesis
    // L1 block so IndexedTraversal is ready for new block signals.
    node.initialize().await;

    // --- Phase 2: Gossip each block into the node. ---
    for block in &blocks {
        node.act_l2_unsafe_gossip_receive(block);
    }

    assert_eq!(
        node.l2_unsafe_number(),
        L2_BLOCK_COUNT,
        "unsafe_head should have advanced to block {L2_BLOCK_COUNT} after gossip"
    );
    assert_eq!(node.l2_safe_number(), 0, "safe_head should still be at genesis before derivation");

    // --- Phase 3+4: Signal L1 head and run derivation. ---
    node.act_l1_head_signal(l1_block_1).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected {L2_BLOCK_COUNT} L2 blocks derived");
    assert_eq!(
        node.l2_safe_number(),
        L2_BLOCK_COUNT,
        "safe_head should have caught up to block {L2_BLOCK_COUNT}"
    );
    assert_eq!(
        node.l2_unsafe_number(),
        node.l2_safe_number(),
        "unsafe_head and safe_head should be equal after safe chain caught up"
    );
}

/// Verify that gossiped blocks arriving out of sequential order are silently
/// dropped and do not advance `unsafe_head`.
///
/// The guard accepts only `block.number == unsafe_head.number + 1`.  Injecting
/// block 3 when `unsafe_head` is at genesis (0) is a gap-jump: 3 ≠ 0 + 1,
/// so the block is dropped and `unsafe_head` stays at 0.
#[tokio::test]
async fn test_out_of_order_gossip_is_dropped() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build 3 sequential L2 blocks (we will inject block 3 first, skipping 1 & 2).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block_with_single_transaction();
    let _block2 = sequencer.build_next_block_with_single_transaction();
    let block3 = sequencer.build_next_block_with_single_transaction();

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Inject block 3 first — gap-jump; must be dropped.
    node.act_l2_unsafe_gossip_receive(&block3);
    assert_eq!(
        node.l2_unsafe_number(),
        0,
        "unsafe_head must not advance when block 3 arrives before blocks 1 and 2"
    );

    // Inject block 1 — sequential; must advance.
    node.act_l2_unsafe_gossip_receive(&block1);
    assert_eq!(
        node.l2_unsafe_number(),
        1,
        "unsafe_head should advance to 1 after sequential gossip of block 1"
    );

    // Inject block 3 again — still a gap (unsafe_head=1, next expected=2); must be dropped.
    node.act_l2_unsafe_gossip_receive(&block3);
    assert_eq!(
        node.l2_unsafe_number(),
        1,
        "unsafe_head must stay at 1 when block 3 arrives before block 2"
    );
}
