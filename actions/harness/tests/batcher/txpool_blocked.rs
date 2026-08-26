//! Action tests for batcher recovery when the L1 txpool nonce slot is blocked.
//!
//! These tests verify the end-to-end production recovery path:
//!   sequencer → batcher (txpool blocked) → requeue + `cancel_tx` → derivation
//!
//! When [`TxManager::submit`] rejects a submission with
//! [`TxManagerError::AlreadyReserved`], the [`BatchDriver`] classifies the
//! outcome as [`TxOutcome::TxpoolBlocked`]: it requeues the frames, stops
//! submitting new ones, and calls [`TxManager::cancel_tx`] on its next loop
//! iteration to clear the stuck slot before resubmitting. This is a distinct
//! driver code path from a plain submission failure (see `submission_failure.rs`),
//! and it exercises the `cancel_tx` hook that the harness previously left as the
//! trait's default no-op.
//!
//! [`TxManager::submit`]: base_tx_manager::TxManager::submit
//! [`TxManager::cancel_tx`]: base_tx_manager::TxManager::cancel_tx
//! [`TxManagerError::AlreadyReserved`]: base_tx_manager::TxManagerError::AlreadyReserved
//! [`BatchDriver`]: base_batcher_core::BatchDriver
//! [`TxOutcome::TxpoolBlocked`]: base_batcher_core::TxOutcome::TxpoolBlocked

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

fn calldata_batcher_config() -> BatcherConfig {
    BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    }
}

/// When the first submission hits a reserved nonce slot, the [`BatchDriver`]
/// requeues the frame, clears the blockage via [`TxManager::cancel_tx`], and the
/// derivation node successfully derives the L2 block after recovery.
///
/// [`BatchDriver`]: base_batcher_core::BatchDriver
/// [`TxManager::cancel_tx`]: base_tx_manager::TxManager::cancel_tx
#[tokio::test]
async fn txpool_blocked_recovers_via_cancel_tx_and_derives() {
    let batcher_cfg = calldata_batcher_config();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg);

    // Block the first submission so the driver must recover the txpool slot
    // before it can resubmit the requeued frame.
    batcher.block_next_n_submissions(1);
    batcher.encode_only().await;

    // Driver path: TxpoolBlocked receipt → requeue → recover_txpool → cancel_tx
    // → resubmit. The frame returns to pending only after the blockage clears.
    batcher.wait_until_requeued(1).await;

    assert_eq!(
        batcher.cancellation_count(),
        1,
        "driver must clear the txpool blockage via exactly one cancel_tx"
    );

    // Mine the successfully resubmitted frame into an L1 block.
    let block_num = batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "frame must derive one L2 block after txpool recovery");
    assert_eq!(node.l2_safe_number(), 1, "safe head must reach 1 after recovery");
    assert!(block_num >= 1, "resubmitted frame was mined into an L1 block");
}

/// With two consecutive txpool blockages, the driver recovers twice via
/// [`TxManager::cancel_tx`] before the third submission succeeds. No data is
/// lost: the derivation node still sees the correct L2 block.
///
/// [`TxManager::cancel_tx`]: base_tx_manager::TxManager::cancel_tx
#[tokio::test]
async fn consecutive_txpool_blocks_recover_and_derive() {
    let batcher_cfg = calldata_batcher_config();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg);

    // Block the next two submissions; the driver must recover after each before
    // the third submission queues successfully.
    batcher.block_next_n_submissions(2);
    batcher.encode_only().await;

    batcher.wait_until_requeued(1).await;

    assert_eq!(
        batcher.cancellation_count(),
        2,
        "driver must clear two consecutive txpool blockages via cancel_tx"
    );

    let block_num = batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "frame must survive two blockages and derive one L2 block");
    assert_eq!(node.l2_safe_number(), 1, "safe head must reach 1 after two recoveries");
    assert!(block_num >= 1, "frame was eventually mined into an L1 block");
}
