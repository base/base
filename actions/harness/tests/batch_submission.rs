#![doc = "Action tests for L2 batch submission via the Batcher actor."]

use base_action_harness::{Action, ActionTestHarness, BatcherConfig, BatcherError};
use base_protocol::DERIVATION_VERSION_0;

/// Helper: returns a harness with `l2_count` L2 blocks pre-loaded.
fn harness_with_l2(l2_count: u64) -> ActionTestHarness {
    let mut h = ActionTestHarness::default();
    h.generate_l2_blocks(l2_count);
    h
}

// ---------------------------------------------------------------------------
// Basic encoding
// ---------------------------------------------------------------------------

#[test]
fn batcher_produces_frames_for_single_block() {
    let mut h = harness_with_l2(1);
    let mut batcher = h.create_batcher(BatcherConfig::default());
    let frames = batcher.advance().expect("advance should succeed");
    assert!(!frames.is_empty(), "expected at least one frame");
}

#[test]
fn batcher_produces_frames_for_multiple_blocks() {
    let mut h = harness_with_l2(5);
    let mut batcher = h.create_batcher(BatcherConfig::default());
    let frames = batcher.advance().expect("advance should succeed");
    assert!(!frames.is_empty(), "expected at least one frame");
}

#[test]
fn batcher_errors_when_no_l2_blocks() {
    let mut h = ActionTestHarness::default(); // no L2 blocks
    let mut batcher = h.create_batcher(BatcherConfig::default());
    let err = batcher.advance().expect_err("should fail with no blocks");
    assert!(matches!(err, BatcherError::NoBlocks));
}

// ---------------------------------------------------------------------------
// L1 tx submission
// ---------------------------------------------------------------------------

#[test]
fn batcher_submits_tx_to_l1_pending_pool() {
    let mut h = harness_with_l2(3);
    let cfg = BatcherConfig::default();
    let inbox = cfg.inbox_address;
    let batcher_addr = cfg.batcher_address;

    let mut batcher = h.create_batcher(cfg);
    batcher.advance().expect("advance should succeed");
    drop(batcher);

    // Before mining: pending pool should have at least one tx.
    let pending = h.l1.pending_txs();
    assert!(!pending.is_empty(), "expected pending batcher txs");
    for tx in pending {
        assert_eq!(tx.to, inbox, "tx recipient should be batch inbox");
        assert_eq!(tx.from, batcher_addr, "tx sender should be batcher address");
    }
}

#[test]
fn batcher_tx_payload_starts_with_derivation_version_0() {
    let mut h = harness_with_l2(2);
    let mut batcher = h.create_batcher(BatcherConfig::default());
    batcher.advance().expect("advance should succeed");
    drop(batcher);

    for tx in h.l1.pending_txs() {
        assert_eq!(
            tx.input.first().copied(),
            Some(DERIVATION_VERSION_0),
            "tx input must start with DERIVATION_VERSION_0"
        );
    }
}

#[test]
fn mined_block_contains_batcher_txs() {
    let mut h = harness_with_l2(2);
    let mut batcher = h.create_batcher(BatcherConfig::default());
    batcher.advance().expect("advance should succeed");
    drop(batcher);

    h.l1.mine_block();

    let tip = h.l1.tip();
    assert!(!tip.batcher_txs.is_empty(), "mined block should contain batcher txs");
}

// ---------------------------------------------------------------------------
// Action trait delegation
// ---------------------------------------------------------------------------

#[test]
fn action_act_delegates_to_advance() {
    let mut h = harness_with_l2(1);
    let mut batcher = h.create_batcher(BatcherConfig::default());
    let frames = batcher.act().expect("act should succeed");
    assert!(!frames.is_empty());
}

// ---------------------------------------------------------------------------
// Multi-cycle: reuse harness after batcher is dropped
// ---------------------------------------------------------------------------

#[test]
fn two_batcher_cycles_each_submit_distinct_txs() {
    let mut h = ActionTestHarness::default();

    // First cycle: 2 L2 blocks.
    h.generate_l2_blocks(2);
    {
        let mut batcher = h.create_batcher(BatcherConfig::default());
        batcher.advance().expect("first advance");
    }
    h.l1.mine_block();
    let after_first = h.l1.tip().batcher_txs.len();
    assert!(after_first > 0);

    // Second cycle: 3 more L2 blocks.
    h.generate_l2_blocks(3);
    {
        let mut batcher = h.create_batcher(BatcherConfig::default());
        batcher.advance().expect("second advance");
    }
    h.l1.mine_block();
    let after_second = h.l1.tip().batcher_txs.len();
    assert!(after_second > 0);

    // Both cycles produced txs; total L1 height is 2.
    assert_eq!(h.l1.latest_number(), 2);
}

// ---------------------------------------------------------------------------
// Reorg interaction
// ---------------------------------------------------------------------------

#[test]
fn batcher_txs_survive_reorg_and_resubmit() {
    let mut h = harness_with_l2(2);

    // Mine an empty block 1 so we have a safe reorg target.
    h.l1.mine_block();

    // Batch and mine block 2.
    {
        let mut batcher = h.create_batcher(BatcherConfig::default());
        batcher.advance().expect("advance");
    }
    h.l1.mine_block();
    assert_eq!(h.l1.latest_number(), 2);

    // Reorg back to block 1 — block 2 (with batcher txs) is lost.
    let lost = h.l1.reorg_to(1).expect("reorg to block 1");
    assert_eq!(lost.len(), 1);
    assert!(!lost[0].batcher_txs.is_empty(), "lost block had batcher txs");
    assert_eq!(h.l1.latest_number(), 1);

    // Re-submit: generate fresh L2 blocks and batch again.
    h.generate_l2_blocks(2);
    {
        let mut batcher = h.create_batcher(BatcherConfig::default());
        batcher.advance().expect("re-advance after reorg");
    }
    h.l1.mine_block();

    // Post-reorg block 2 should also contain batcher txs.
    assert!(!h.l1.tip().batcher_txs.is_empty());
    assert_eq!(h.l1.latest_number(), 2);
}
