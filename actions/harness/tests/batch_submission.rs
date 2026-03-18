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

/// An L1 reorg fires failure receipts for frames that were staged but not yet
/// confirmed, causing the [`BatchDriver`] to requeue them in the encoder
/// pipeline and resubmit on the new fork — **without creating a new
/// [`Batcher`]**.
///
/// Sequence:
/// 1. Encode and stage all frames; mine L1 block 1 (original).
/// 2. Reorg to genesis **before** calling `confirm_staged` — frames are still
///    in `staged`, so `reorg_to` fires `Err(TxManagerError::Rpc("reorg"))` on
///    each oneshot responder.
/// 3. The driver processes each `Receipt(id, Failed)` → `pipeline.requeue(id)`
///    rewinds the encoder channel cursor. On the next loop iteration, the driver
///    calls `submit_pending()` → `send_async()` and the frames are back in the
///    `L1MinerTxManager` pending queue.
/// 4. The same batcher stages the requeued frames and mines a new L1 block on
///    the new fork. The verifier re-derives L2 block 1 from this block.
///
/// [`BatchDriver`]: base_batcher_core::BatchDriver
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

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode and stage all frames; mine L1 block 1 (original).
    // Do NOT call confirm_staged — frames remain in `staged` so the reorg
    // below fires failure receipts for them.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg);
    batcher.encode_only().await.expect("encode");
    batcher.stage_n_frames(&mut h.l1, usize::MAX);
    h.l1.mine_block(); // L1 block 1 (original, about to be reorged)
    chain.push(h.l1.tip().clone());

    // --- L1 reorg back to genesis (frames still in staged) ---
    // reorg_to fires Err(TxManagerError::Rpc("reorg")) for every staged item
    // and sends L1HeadEvent::NewHead(0). The driver's select! loop processes
    // each Receipt(id, Failed) → pipeline.requeue(id), rewinding the channel
    // cursor without re-encoding.
    batcher.reorg(0, &mut h.l1);
    batcher.wait_until_requeued(1).await.expect("driver must requeue frames after reorg");

    // Mine an empty replacement block on the new fork, then resubmit the
    // requeued frames using the same Batcher (no drop/recreate required).
    h.l1.mine_block(); // block 1' (empty, on new fork)
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    batcher.stage_n_frames(&mut h.l1, usize::MAX);
    let recovery_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(recovery_num).await;

    // Verify the verifier re-derives L2 block 1 from the new-fork submission.
    verifier.initialize().await.expect("initialize");

    let blk_1_prime = block_info_from(h.l1.block_by_number(1).expect("block 1'"));
    verifier.act_l1_head_signal(blk_1_prime).await.expect("signal empty block 1'");
    let empty = verifier.act_l2_pipeline_full().await.expect("step empty block 1'");
    assert_eq!(empty, 0, "empty block 1' has no batch data");

    let recovery_blk = block_info_from(h.l1.block_by_number(recovery_num).expect("recovery block"));
    verifier.act_l1_head_signal(recovery_blk).await.expect("signal recovery block");
    let derived = verifier.act_l2_pipeline_full().await.expect("step recovery");
    assert_eq!(derived, 1, "same-batcher resubmission must derive L2 block 1");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        1,
        "safe head must recover to 1 after same-batcher resubmission on new fork"
    );
}
