#![doc = "Action tests for L2 batch submission via the Batcher actor."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatchType, Batcher, BatcherConfig, BatcherError, DaType,
    EncoderConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};

/// Build an [`ActionL2Source`] pre-populated with `n` real [`OpBlock`]s from
/// the genesis of the given harness.
///
/// [`OpBlock`]: base_alloy_consensus::OpBlock
fn make_source(h: &ActionTestHarness, n: u64) -> ActionL2Source {
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(chain);
    let mut source = ActionL2Source::new();
    for _ in 0..n {
        source.push(sequencer.build_next_block().expect("build L2 block"));
    }
    source
}

// ---------------------------------------------------------------------------
// Batcher: persistent pipeline end-to-end path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batcher_mines_block_with_submissions() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig::default();

    let source = make_source(&h, 3);
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    batcher.advance(&mut h.l1).await.expect("advance should succeed");

    assert!(h.l1.latest_number() >= 1, "at least one L1 block should be mined");
    // Default EncoderConfig uses DaType::Blob, so submissions appear as blob sidecars.
    assert!(
        !h.l1.tip().batcher_txs.is_empty() || !h.l1.tip().blob_sidecars.is_empty(),
        "mined block should contain batcher submissions (calldata or blobs)"
    );
}

#[tokio::test]
async fn batcher_span_batch_mode() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig { batch_type: BatchType::Span, ..Default::default() };

    let source = make_source(&h, 3);
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    batcher.advance(&mut h.l1).await.expect("advance span should succeed");

    assert!(h.l1.latest_number() >= 1, "at least one L1 block should be mined");
    assert!(
        !h.l1.tip().batcher_txs.is_empty() || !h.l1.tip().blob_sidecars.is_empty(),
        "mined block should contain span batcher submissions (calldata or blobs)"
    );
}

#[tokio::test]
async fn batcher_errors_when_no_l2_blocks_async() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig::default();

    let source = ActionL2Source::new(); // empty
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    let err = batcher.advance(&mut h.l1).await.expect_err("should fail with no blocks");
    assert!(matches!(err, BatcherError::NoBlocks));
}

// ---------------------------------------------------------------------------
// Batcher: L1 reorg during submission
// ---------------------------------------------------------------------------

/// An L1 reorg discards a block containing batcher submissions and truncates
/// the canonical chain. After recovery — creating a new `Batcher` and
/// resubmitting the same L2 data on the new fork — the verifier re-derives
/// the L2 block and advances safe head back to 1.
///
/// Note: in this test the reorg is called after `confirm_staged` has already
/// drained all staged items, so `reorg_to` fires zero failure receipts. The
/// test covers L1 chain truncation and re-derivation on a new fork. A test that
/// exercises the failure-receipt path (reorg called while items are still in
/// `staged`) would require calling `reorg` between `stage_n_frames` and
/// `confirm_staged`.
#[tokio::test]
async fn batcher_reorg_during_submission() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build L2 block 1");

    // Create a verifier sharing the sequencer's block-hash registry.
    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Batcher: encode + stage + mine L1 block 1.
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await.expect("encode");
    batcher.stage_n_frames(&mut h.l1, usize::MAX);
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

    // Derive through the verifier: safe head should advance to 1.
    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("step block 1");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        1,
        "safe head should be 1 after initial submission"
    );

    // --- L1 reorg back to genesis ---
    batcher.reorg(0, &mut h.l1);
    // Give the driver several scheduling turns to process the L1HeadEvent::NewHead(0)
    // that reorg() sends via the l1_head channel. The reorg fires 0 failure receipts
    // here (pending and staged are both empty after confirm_staged), so the driver
    // only needs to process the head event. A single yield suffices on a
    // current_thread runtime; the extra yields guard against any scheduling races
    // when running under other executor configurations.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Mine an empty replacement block on the new fork. This gives the pipeline
    // an L1 block to advance to after reset — without it the pipeline loops
    // infinitely between AdvancedOrigin (at genesis, resetting no_progress)
    // and NotEnoughData (trying to fetch the now-missing old block 1).
    h.l1.mine_block(); // block 1' (empty, on new fork)
    let l1_block_1_prime = block_info_from(h.l1.tip());
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    // Reset the verifier pipeline to genesis.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis"));
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = h.rollup_config.genesis.system_config.unwrap_or_default();
    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    // Drain the reset origin (genesis has no batch data).
    let genesis_derived = verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");
    assert_eq!(genesis_derived, 0, "genesis drain after reset must derive no L2 blocks");

    // Step over the empty block 1' — nothing derived.
    verifier.act_l1_head_signal(l1_block_1_prime).await.expect("signal empty block 1'");
    let empty = verifier.act_l2_pipeline_full().await.expect("step empty block 1'");
    assert_eq!(empty, 0, "empty block 1' has no batch data");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "safe head should revert to genesis after reorg"
    );

    // --- Recovery: new Batcher resubmits on the new fork ---
    // Drop the old batcher before creating batcher2. `Drop` cancels the token
    // and calls `JoinHandle::abort()`, which *schedules* the abort but does not
    // await it. On a current_thread runtime the old task is only actually polled
    // to completion on the next yield inside batcher2's encode_only(). This is
    // safe because batcher2 constructs a fresh L1MinerTxManager (new Arc), so
    // there is no shared mutable state between the two driver tasks.
    drop(batcher);
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    let mut batcher2 = Batcher::new(source2, &h.rollup_config, batcher_cfg);
    batcher2.advance(&mut h.l1).await.expect("recovery advance");
    chain.push(h.l1.tip().clone());

    let recovery_block =
        block_info_from(h.l1.block_by_number(h.l1.latest_number()).expect("recovery block"));
    verifier.act_l1_head_signal(recovery_block).await.expect("signal recovery block");
    let derived = verifier.act_l2_pipeline_full().await.expect("step recovery");
    assert_eq!(derived, 1, "recovery channel should derive L2 block 1");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        1,
        "safe head should recover to 1 after resubmission on new fork"
    );
}
