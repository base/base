#![doc = "Action tests for L2 finalization via the verifier pipeline."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, block_info_from,
};
use base_consensus_genesis::RollupConfig;
use base_consensus_registry::Registry;

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// Mirrors the helper used across the `derivation.rs` tests.
fn rollup_config_for(batcher: &BatcherConfig) -> RollupConfig {
    let mut rc = Registry::rollup_config(8453).expect("mainnet config").clone();
    rc.batch_inbox_address = batcher.inbox_address;
    rc.genesis.system_config.as_mut().unwrap().batcher_address = batcher.batcher_address;
    rc.genesis.l2_time = 0;
    rc.genesis.l1 = Default::default();
    rc.genesis.l2 = Default::default();
    rc.hardforks.canyon_time = Some(0);
    rc.hardforks.delta_time = Some(0);
    rc.hardforks.ecotone_time = Some(0);
    rc.hardforks.fjord_time = Some(0);
    rc
}

/// When multiple L2 blocks share the same L1 epoch (`l1_origin`), finalizing the
/// L1 inclusion block causes ALL L2 blocks in that epoch to become finalized
/// together. The finalized head should advance to the highest L2 block whose
/// L1 origin is at or before the finalized L1 number.
#[tokio::test]
async fn finalization_advances_with_multiple_l2_blocks_per_epoch() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build 3 L2 blocks, all referencing L1 epoch 0 (genesis).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::new();
    let mut block_hashes = Vec::new();
    for _ in 0..3 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let head = sequencer.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        blocks.push(block);
    }
    // All blocks should reference epoch 0.
    assert_eq!(sequencer.head().l1_origin.number, 0, "all blocks should be in epoch 0");

    // Submit each block in a separate L1 inclusion block.
    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // Create verifier after mining and register block hashes.
    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    // Finalized head starts at genesis.
    assert_eq!(verifier.l2_finalized().block_info.number, 0);

    // Derive all 3 L2 blocks.
    for i in 1..=3u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 3, "safe head should reach L2 block 3");

    // Signal that L1 block 1 is finalized. All 3 L2 blocks have l1_origin = 0,
    // so l1_origin(0) <= finalized_l1(1) and all become finalized together.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_finalized_signal(l1_block_1).await.expect("finalized signal");

    assert_eq!(
        verifier.l2_finalized().block_info.number,
        3,
        "all 3 L2 blocks in epoch 0 should finalize when L1 block 1 is finalized"
    );
}

/// L2 finalization advances incrementally as successive L1 epochs are finalized.
///
/// Produces L2 blocks across two L1 epochs (epoch 0 and epoch 1). Finalizing the
/// epoch-0 L1 block first advances the finalized head only through the last L2 block
/// whose `l1_origin` is 0, leaving the epoch-1 block pending. Finalizing the epoch-1
/// L1 block then advances it through the remaining block — demonstrating that the
/// finalized L2 head advances one epoch at a time as each successive L1 epoch is
/// finalized.
#[tokio::test]
async fn finalization_advances_incrementally_with_l1_epochs() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 block 1 so the sequencer can advance to epoch 1.
    // L1 block_time = 12, so L1 block 1 has timestamp 12.
    // With L2 block_time = 2, L2 blocks 1-5 (ts 2-10) reference epoch 0,
    // and L2 block 6 (ts 12) advances to epoch 1.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 6 L2 blocks: blocks 1-5 in epoch 0, block 6 in epoch 1.
    let mut blocks = Vec::new();
    let mut block_hashes = Vec::new();
    let mut last_epoch_0_number = 0u64;
    for i in 1..=6u64 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let head = sequencer.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        blocks.push(block);
        if head.l1_origin.number == 0 {
            last_epoch_0_number = i;
        }
    }
    assert_eq!(sequencer.head().l1_origin.number, 1, "last L2 block should reference epoch 1");
    assert!(last_epoch_0_number > 0, "at least one L2 block should reference epoch 0");

    let (mut verifier, chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }

    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    verifier.initialize().await.expect("initialize");

    // Signal and derive all L1 blocks: block 1 is the epoch-providing block,
    // blocks 2-7 contain batches.
    for i in 1..=(1 + 6) {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 6, "safe head should reach L2 block 6");

    // First finalization signal: L1 block 0 (epoch 0). The `L2Finalizer` tracks
    // each L2 block by its `derived_from` L1 origin, so only blocks with
    // `l1_origin = 0` are covered. Block 6 (`l1_origin = 1`) must stay pending.
    let l1_epoch_0 = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    verifier.act_l1_finalized_signal(l1_epoch_0).await.expect("finalize epoch 0");
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        last_epoch_0_number,
        "first signal (epoch 0): only epoch-0 blocks should finalize"
    );
    assert!(
        verifier.l2_finalized().block_info.number < 6,
        "epoch-1 block (L2 block 6) must not yet be finalized"
    );

    // Second finalization signal: L1 block 1 (epoch 1). Now block 6 finalizes.
    let l1_epoch_1 = block_info_from(h.l1.block_by_number(1).expect("L1 block 1"));
    verifier.act_l1_finalized_signal(l1_epoch_1).await.expect("finalize epoch 1");
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        6,
        "second signal (epoch 1): block 6 should now be finalized"
    );
}

/// The finalized L2 head must never exceed the safe head, even when the L1
/// finalized signal references a block far ahead of what has been derived.
#[tokio::test]
async fn finalization_does_not_exceed_safe_head() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build 2 L2 blocks in epoch 0.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let block1 = sequencer.build_next_block().expect("build block 1");
    let hash1 = sequencer.head().block_info.hash;
    let block2 = sequencer.build_next_block().expect("build block 2");
    let hash2 = sequencer.head().block_info.hash;

    // Submit each block via the batcher.
    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // Mine many more L1 blocks without any corresponding L2 derivation data.
    h.mine_l1_blocks(10);

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.initialize().await.expect("initialize");

    // Derive only 2 L2 blocks.
    for i in 1..=2u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2, "safe head should be 2");

    // Signal an L1 finalized block FAR beyond what's been derived (block 12).
    // The finalization logic only looks at safe_head_history, so it cannot
    // finalize anything beyond what has been derived.
    let l1_far_ahead = block_info_from(h.l1.block_by_number(12).expect("block 12"));
    verifier.act_l1_finalized_signal(l1_far_ahead).await.expect("finalized signal");

    assert!(
        verifier.l2_finalized().block_info.number <= verifier.l2_safe().block_info.number,
        "finalized head ({}) must never exceed safe head ({})",
        verifier.l2_finalized().block_info.number,
        verifier.l2_safe().block_info.number,
    );
    // Both L2 blocks have l1_origin = 0, which is <= 12, so they should
    // finalize to the highest derived block.
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        2,
        "finalized head should be capped at safe head (2)"
    );
}

/// After a pipeline reset (simulating a reorg), finalization state is cleared
/// back to genesis. After re-deriving blocks, finalization can proceed again.
#[tokio::test]
async fn finalization_reorg_clears_state() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build 2 L2 blocks.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let block1 = sequencer.build_next_block().expect("build block 1");
    let hash1 = sequencer.head().block_info.hash;
    let block2 = sequencer.build_next_block().expect("build block 2");
    let hash2 = sequencer.head().block_info.hash;

    // Submit and mine.
    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.initialize().await.expect("initialize");

    // Derive both L2 blocks.
    for i in 1..=2u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2);

    // Finalize L1 block 1 → L2 finalized should advance.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_finalized_signal(l1_block_1).await.expect("finalized signal");
    assert_eq!(verifier.l2_finalized().block_info.number, 2, "pre-reset finalized = 2");

    // Simulate a reorg by resetting the pipeline to genesis.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");

    // After reset, finalized head should be back to genesis (block 0).
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        0,
        "finalized head should reset to genesis after pipeline reset"
    );

    // Re-derive by re-signalling the existing L1 blocks. The L1 chain is still
    // intact (no actual L1 reorg happened), so re-derive same blocks.
    // Re-mine a new fork block for the reset pipeline.
    h.l1.reorg_to(0).expect("reorg to genesis");
    // Build a new L2 block on the fresh fork.
    let l1_chain_fresh = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer_fresh = h.create_l2_sequencer(l1_chain_fresh);
    let block1_fresh = sequencer_fresh.build_next_block().expect("build fresh block 1");
    let hash1_fresh = sequencer_fresh.head().block_info.hash;

    let mut source = ActionL2Source::new();
    source.push(block1_fresh);
    let mut batcher_fresh = h.create_batcher(source, batcher_cfg.clone());
    batcher_fresh.advance().expect("encode fresh");
    drop(batcher_fresh);
    h.l1.mine_block();

    // Push the new block to the shared chain.
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    verifier.register_block_hash(1, hash1_fresh);

    let l1_block_1_new = block_info_from(h.l1.block_by_number(1).expect("new block 1"));
    verifier.act_l1_head_signal(l1_block_1_new).await.expect("signal new block 1");
    verifier.act_l2_pipeline_full().await.expect("derive after reset");

    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head re-derived to 1");

    // Finalize the new L1 block 1 → finalization works again.
    verifier.act_l1_finalized_signal(l1_block_1_new).await.expect("finalized after reset");
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        1,
        "finalization should work cleanly after reset and re-derivation"
    );
}

/// Once L2 finalization reaches block N, signalling an older L1 block as
/// finalized must not regress the finalized L2 head.
#[tokio::test]
async fn finalization_does_not_regress() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 block 1 for epoch advancement.
    h.mine_l1_blocks(1);

    // Build 6 L2 blocks: blocks 1-5 in epoch 0, block 6 in epoch 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::new();
    let mut block_hashes = Vec::new();
    for _ in 0..6 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let head = sequencer.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        blocks.push(block);
    }

    // Submit each L2 block in a separate L1 inclusion block.
    let (mut verifier, chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }

    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    verifier.initialize().await.expect("initialize");

    // Derive all L2 blocks. L1 block 1 is epoch-providing, blocks 2-7 have batches.
    for i in 1..=(1 + 6) {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 6, "safe head should be 6");

    // Finalize L1 block 1. All L2 blocks with l1_origin <= 1 are finalized.
    // Since epoch 0 blocks have l1_origin 0 and epoch 1 block has l1_origin 1,
    // all 6 blocks should finalize.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("L1 block 1"));
    verifier.act_l1_finalized_signal(l1_block_1).await.expect("finalized at L1 block 1");
    let finalized_after_first = verifier.l2_finalized().block_info.number;
    assert_eq!(finalized_after_first, 6, "all 6 blocks should be finalized");

    // Now signal an OLDER L1 block (genesis, block 0) as finalized.
    // This should NOT cause the finalized head to regress.
    let l1_genesis = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    verifier.act_l1_finalized_signal(l1_genesis).await.expect("older finalized signal");

    assert_eq!(
        verifier.l2_finalized().block_info.number,
        finalized_after_first,
        "finalized head must not regress when an older L1 block is signalled as finalized"
    );
}
