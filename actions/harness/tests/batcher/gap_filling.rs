//! Action tests for derivation across gaps in submitted L2 blocks.
//!
//! Batches submitted after a gap cannot advance the verifier's safe head.
//! Submitting the missing sequence fills the gap, and duplicate later blocks
//! remain harmless.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Batches posted after a gap do not advance the safe head, batches filling the gap do, and
/// the ones posted twice are harmless.
///
/// Each phase posts its blocks from a fresh batcher, which starts from the parent of the
/// first block it is given: in phase 2 that is block 7, as if its node were ahead of the
/// verifier.
#[tokio::test]
async fn batcher_gap_fill_separate_instances() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::with_capacity(10);
    for _ in 0..10 {
        blocks.push(sequencer.build_next_block_with_single_transaction().await);
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // ----- Phase 1: post blocks 1-5 -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[..5] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 1: expected 5 L2 blocks derived");
    assert_eq!(node.l2_safe_number(), 5, "Phase 1: safe head must be 5");

    // ----- Phase 2: a fresh batcher posts blocks 8-10 (gap) -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[7..10] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    let derived = node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        5,
        "Phase 2: safe head must remain at 5 — gap blocks 6-7 are missing"
    );
    assert_eq!(derived, 0, "Phase 2: no blocks derived (gap)");

    // ----- Phase 3: a fresh batcher posts blocks 6-10 -----
    {
        let mut source = ActionL2Source::new();
        for block in &blocks[5..10] {
            source.push(block.clone());
        }
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 5, "Phase 3: expected 5 L2 blocks derived (6-10)");
    assert_eq!(node.l2_safe_number(), 10, "Phase 3: safe head must reach 10 after gap is filled");
}
