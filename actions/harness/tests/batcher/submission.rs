//! Action tests for L2 batch submission via the Batcher actor.

use std::sync::Arc;

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, BatcherError, L1MinerConfig,
    SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{BatchEncoder, BatchPipeline, CompressionAlgo, DaType, EncoderConfig};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_protocol::{BatchType, Frame};

/// Return the compressed payload size for one unbounded Span channel.
fn span_channel_size(rollup_config: &RollupConfig, blocks: &[BaseBlock]) -> usize {
    let config = EncoderConfig {
        batch_type: BatchType::Span,
        compression_algo: CompressionAlgo::Brotli10,
        max_blocks_per_span_batch: Some(1),
        ..EncoderConfig::default()
    };
    let mut encoder = BatchEncoder::new(Arc::new(rollup_config.clone()), config);
    for block in blocks {
        encoder.add_block(block.clone()).expect("queue block for size probe");
    }

    let frames = encoder.encode_and_drain().expect("encode size probe");
    assert!(!frames.is_empty(), "size probe must produce a frame");

    // Brotli framing adds one channel-version byte outside the compressed payload.
    frames.iter().map(|frame| frame.data.len()).sum::<usize>() - 1
}

// ---------------------------------------------------------------------------
// Batcher: persistent pipeline end-to-end path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batcher_mines_block_with_submissions() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig::default();

    let source = h.create_l2_source(3).await;
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    batcher.advance(&mut h.l1).await;

    assert!(h.l1.latest_number() >= 1, "at least one L1 block should be mined");
    // Default EncoderConfig uses DaType::Blob, so submissions appear as blob sidecars.
    assert!(
        !h.l1.tip().transactions.is_empty() || !h.l1.tip().blob_sidecars.is_empty(),
        "mined block should contain signed batcher submissions"
    );
}

#[tokio::test]
async fn batcher_span_batch_mode() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig { batch_type: BatchType::Span, ..Default::default() };

    let source = h.create_l2_source(3).await;
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    batcher.advance(&mut h.l1).await;

    assert!(h.l1.latest_number() >= 1, "at least one L1 block should be mined");
    assert!(
        !h.l1.tip().transactions.is_empty() || !h.l1.tip().blob_sidecars.is_empty(),
        "mined block should contain signed span batcher submissions"
    );
}

/// A Span channel that fits one block but rejects the next must close, retry the
/// unchanged block in a fresh channel, and still derive the sequencer's exact chain.
///
/// `max_blocks_per_span_batch = 1` also exercises sealing multiple Span batches
/// inside the rejected candidate rather than closing the channel at each seal.
#[tokio::test]
async fn batcher_span_rejection_retry_derives_exact_blocks() {
    let base_config = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig {
            max_blocks_per_span_batch: Some(1),
            batch_type: BatchType::Span,
            da_type: DaType::Calldata,
            compression_algo: CompressionAlgo::Brotli10,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_config = TestRollupConfigBuilder::base_mainnet(&base_config).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_config);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::with_capacity(4);
    for _ in 0..4 {
        blocks.push(sequencer.build_next_block_with_single_transaction().await);
    }

    let one_block_size = span_channel_size(&h.rollup_config, &blocks[..1]);
    let two_block_size = span_channel_size(&h.rollup_config, &blocks[..2]);
    let target_output_size = one_block_size + 1;
    assert!(
        two_block_size > target_output_size,
        "test requires the second block to exceed the channel target"
    );

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    let encoder = EncoderConfig {
        target_frame_size: Frame::ENCODED_OVERHEAD + 1 + target_output_size,
        ..base_config.encoder.clone()
    };
    let config = BatcherConfig { encoder, ..base_config };
    let mut source = ActionL2Source::new();
    for block in &blocks {
        source.push(block.clone());
    }

    Batcher::new(source, &h.rollup_config, config).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 4, "all rejected-and-retried blocks must derive");
    assert_eq!(node.l2_safe_number(), 4, "safe head must reach the sequencer tip");
    for block in blocks {
        assert_eq!(
            node.derived_block_hash(block.header.number).expect("derived block hash"),
            block.header.hash_slow(),
            "derived hash must match the sequencer at block {}",
            block.header.number
        );
    }
}

#[tokio::test]
async fn batcher_errors_when_no_l2_blocks_async() {
    let mut h = ActionTestHarness::default();
    let cfg = BatcherConfig::default();

    let source = ActionL2Source::new(); // empty
    let mut batcher = Batcher::new(source, &h.rollup_config, cfg);
    let err = batcher.try_advance(&mut h.l1).await.expect_err("should fail with no blocks");
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
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode and stage all frames; mine L1 block 1 (original).
    // Do NOT call confirm_staged — frames remain in `staged` so the reorg
    // below fires failure receipts for them.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;
    batcher.stage_n_frames(&mut h.l1, usize::MAX);
    h.l1.mine_block(); // L1 block 1 (original, about to be reorged)
    chain.push(h.l1.tip().clone());

    // --- L1 reorg back to genesis (frames still in staged) ---
    // reorg_to fires Err(TxManagerError::Rpc("reorg")) for every staged item
    // and sends L1HeadEvent::NewHead(0). The driver's select! loop processes
    // each Receipt(id, Failed) → pipeline.requeue(id), rewinding the channel
    // cursor without re-encoding.
    batcher.reorg(0, &mut h.l1);
    batcher.wait_until_requeued(1).await;

    // Mine an empty replacement block on the new fork, then resubmit the
    // requeued frames using the same Batcher (no drop/recreate required).
    h.l1.mine_block(); // block 1' (empty, on new fork)
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    batcher.stage_n_frames(&mut h.l1, usize::MAX);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(h.l1.tip()).await;

    // Verify the node re-derives L2 block 1 from the new-fork submission.
    node.initialize().await;

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "same-batcher resubmission must derive L2 block 1");
    assert_eq!(
        node.l2_safe_number(),
        1,
        "safe head must recover to 1 after same-batcher resubmission on new fork"
    );
}
