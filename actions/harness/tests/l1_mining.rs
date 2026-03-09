#![doc = "Action tests for L1 block mining."]

use base_action_harness::{Action, ActionTestHarness, L1MinerConfig, PendingTx};
use alloy_primitives::{Address, Bytes};

/// Mine a single block and verify the chain advances.
#[test]
fn mine_one_block() {
    let mut h = ActionTestHarness::default();
    h.l1.mine_block();
    assert_eq!(h.l1.latest_number(), 1);
}

/// The harness convenience method mines the requested number of blocks.
#[test]
fn mine_n_blocks_via_harness() {
    let mut h = ActionTestHarness::default();
    let latest = h.mine_l1_blocks(5);
    assert_eq!(latest, 5);
    assert_eq!(h.l1.latest_number(), 5);
}

/// Block numbers increment by one and timestamps by block_time each step.
#[test]
fn blocks_advance_number_and_timestamp() {
    let mut h = ActionTestHarness::default();
    let block_time = 12u64;

    for n in 1..=4 {
        h.l1.mine_block();
        let b = h.l1.latest();
        assert_eq!(b.number(), n);
        assert_eq!(b.timestamp(), n * block_time);
    }
}

/// Each block's parent_hash matches the hash of the preceding block.
#[test]
fn parent_hash_chain_is_valid() {
    let mut h = ActionTestHarness::default();
    h.mine_l1_blocks(4);

    for n in 1..=4u64 {
        let block = h.l1.block_by_number(n).unwrap();
        let parent = h.l1.block_by_number(n - 1).unwrap();
        assert_eq!(block.header.parent_hash, parent.hash(), "block {n} has wrong parent_hash");
    }
}

/// Safe head lags 32 blocks behind latest; finalized lags 64.
#[test]
fn safe_and_finalized_heads_lag_correctly() {
    let mut h = ActionTestHarness::default();

    // Before the lag window: both heads stay at genesis.
    h.mine_l1_blocks(10);
    assert_eq!(h.l1.safe_head().number(), 0);
    assert_eq!(h.l1.finalized_head().number(), 0);

    // Once we pass the safe lag of 32, safe head advances.
    h.mine_l1_blocks(25); // total = 35
    assert_eq!(h.l1.safe_head().number(), 35u64.saturating_sub(32));

    // Once we pass finalized lag of 64, finalized head advances.
    h.mine_l1_blocks(30); // total = 65
    assert_eq!(h.l1.finalized_head().number(), 65u64.saturating_sub(64));
}

/// The Action trait impl returns the mined block number.
#[test]
fn action_impl_returns_block_number() {
    let mut h = ActionTestHarness::default();
    assert_eq!(h.l1.act().unwrap(), 1);
    assert_eq!(h.l1.act().unwrap(), 2);
    assert_eq!(h.l1.act().unwrap(), 3);
}

/// Batcher transactions submitted before mining appear in the block body.
#[test]
fn batcher_txs_included_in_mined_block() {
    let batcher_addr = Address::repeat_byte(0xAB);
    let inbox_addr = Address::repeat_byte(0xCD);
    let frame_data = Bytes::from_static(b"\x00frame_bytes_here");

    let mut h = ActionTestHarness::default();
    h.l1.submit_tx(PendingTx { from: batcher_addr, to: inbox_addr, input: frame_data.clone() });
    h.l1.mine_block();

    let block = h.l1.latest();
    assert_eq!(block.batcher_txs.len(), 1);
    assert_eq!(block.batcher_txs[0].from, batcher_addr);
    assert_eq!(block.batcher_txs[0].to, inbox_addr);
    assert_eq!(block.batcher_txs[0].input, frame_data);
}

/// Pending transactions are cleared after each block — they don't carry over.
#[test]
fn batcher_txs_do_not_carry_over_to_next_block() {
    let mut h = ActionTestHarness::default();
    h.l1.submit_tx(PendingTx {
        from: Address::ZERO,
        to: Address::ZERO,
        input: Bytes::new(),
    });
    h.l1.mine_block(); // consumes the tx
    h.l1.mine_block(); // nothing pending

    assert!(h.l1.latest().batcher_txs.is_empty());
}

// ── Reorg scenarios ───────────────────────────────────────────────────────────

/// Reorging back to block N removes all later blocks from the canonical chain.
#[test]
fn reorg_shortens_chain() {
    let mut h = ActionTestHarness::default();
    h.mine_l1_blocks(5);

    let discarded = h.l1.reorg_to(2).unwrap();
    assert_eq!(discarded.len(), 3); // blocks 3, 4, 5
    assert_eq!(h.l1.latest_number(), 2);
}

/// Batcher transactions included in reorged-out blocks are returned to the
/// caller and are no longer part of the canonical chain.
#[test]
fn reorg_surfaces_lost_batcher_txs() {
    let batcher = Address::repeat_byte(0xBA);
    let inbox = Address::repeat_byte(0xCA);
    let frame = Bytes::from_static(b"\x00reorged_frame");

    let mut h = ActionTestHarness::default();
    h.mine_l1_blocks(2); // blocks 1–2, no batches

    // Submit a batch and mine it into block 3.
    h.l1.submit_tx(PendingTx { from: batcher, to: inbox, input: frame.clone() });
    h.l1.mine_block(); // block 3 contains the batch

    h.mine_l1_blocks(2); // blocks 4–5, empty

    // Reorg back to block 2. Blocks 3–5 are discarded; block 3 held the batch.
    let discarded = h.l1.reorg_to(2).unwrap();
    assert_eq!(h.l1.latest_number(), 2);
    let batch_block = discarded.iter().find(|b| b.number() == 3).unwrap();
    assert_eq!(batch_block.batcher_txs.len(), 1);
    assert_eq!(batch_block.batcher_txs[0].input, frame);

    // Canonical chain no longer has any batcher txs.
    let canonical_txs: usize = h.l1.chain().iter().map(|b| b.batcher_txs.len()).sum();
    assert_eq!(canonical_txs, 0);
}

/// After a reorg, mining continues from the new tip and the parent-hash
/// chain remains valid across the fork point.
#[test]
fn mine_after_reorg_extends_correct_tip() {
    let mut h = ActionTestHarness::default();
    h.mine_l1_blocks(4);

    h.l1.reorg_to(2).unwrap(); // chain: 0–1–2
    h.mine_l1_blocks(3);       // chain: 0–1–2–3'–4'–5'

    assert_eq!(h.l1.latest_number(), 5);

    // Parent-hash chain must be intact across the fork point.
    for n in 1..=5u64 {
        let b = h.l1.block_by_number(n).unwrap();
        let p = h.l1.block_by_number(n - 1).unwrap();
        assert_eq!(b.header.parent_hash, p.hash(), "parent chain broken at block {n}");
    }
}

/// Blocks mined on the new fork have different hashes than the original
/// blocks at the same heights, preventing derivation confusion.
#[test]
fn post_reorg_blocks_differ_from_originals() {
    let mut h = ActionTestHarness::default();
    h.mine_l1_blocks(3);
    let original_3 = h.l1.block_by_number(3).unwrap().hash();

    h.l1.reorg_to(2).unwrap();
    h.l1.mine_block(); // new block 3 on the fork

    let forked_3 = h.l1.block_by_number(3).unwrap().hash();
    assert_ne!(forked_3, original_3, "forked block 3 must have a different hash");
}

/// A batcher can resubmit a transaction that was reorged out, and it will
/// appear in the next mined block on the new fork.
#[test]
fn resubmit_after_reorg_lands_on_new_fork() {
    let from = Address::repeat_byte(0x01);
    let to = Address::repeat_byte(0x02);
    let input = Bytes::from_static(b"\x00resubmitted");

    let mut h = ActionTestHarness::default();
    h.l1.submit_tx(PendingTx { from, to, input: input.clone() });
    h.l1.mine_block(); // block 1 contains the batch
    let original_tx_hash = h.l1.block_by_number(1).unwrap().batcher_txs[0].input.clone();

    // Mine a second block and reorg it away, simulating a short reorg while
    // keeping the first (batch-carrying) block on the canonical chain.
    h.l1.mine_block(); // block 2
    h.l1.reorg_to(1).unwrap(); // discard block 2

    // Resubmit the same frame data on the new fork.
    h.l1.submit_tx(PendingTx { from, to, input: input.clone() });
    h.l1.mine_block(); // new block 2 on fork 1

    assert_eq!(h.l1.latest_number(), 2);
    assert_eq!(h.l1.latest().batcher_txs[0].input, original_tx_hash);
}

/// Configurable block time is respected.
#[test]
fn custom_block_time_is_respected() {
    let mut h = ActionTestHarness::new(
        L1MinerConfig { block_time: 6 },
        Default::default(),
    );
    h.mine_l1_blocks(3);
    assert_eq!(h.l1.latest().timestamp(), 18); // 3 × 6s
}
