#![doc = "Action tests for L2 batch submission via the Batcher actor."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatchType, Batcher, BatcherConfig, BatcherError,
    SharedL1Chain,
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
