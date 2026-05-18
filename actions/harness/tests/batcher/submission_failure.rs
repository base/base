//! Action tests for batcher recovery when L1 frame submission fails.
//!
//! These tests verify the end-to-end path:
//!   sequencer → batcher (submission fails) → batcher requeues → derivation
//!
//! The [`BatchDriver`] reacts to a failed [`SendHandle`] by calling
//! `pipeline.requeue_frame()` and retrying on the next loop iteration.
//! The [`L1MinerTxManager`]'s `fail_next_n` mechanism fires an immediate
//! [`TxManagerError::Rpc`] receipt so this path is exercised without any
//! real L1 interaction.

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

/// When the first frame submission fails, the [`BatchDriver`] requeues the
/// frame, retries, and the derivation node successfully derives the L2 block.
#[tokio::test]
async fn submission_failure_requeues_and_derivation_recovers() {
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

    // Fail the first submission so the driver must requeue and retry.
    batcher.fail_next_n_submissions(1);
    batcher.encode_only().await;

    // Wait for the driver to process the failure receipt and return the frame
    // to the pending queue via requeue + re-submit.
    batcher.wait_until_requeued(1).await;

    // Mine the successfully requeued frame into an L1 block.
    let block_num = batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "requeued frame must produce one derived L2 block");
    assert_eq!(node.l2_safe_number(), 1, "safe head must reach 1 after recovery");
    assert!(block_num >= 1, "requeued frame was mined into an L1 block");
}

/// With three consecutive submission failures the [`BatchDriver`] requeues and
/// retries three times before the fourth attempt succeeds. No data is lost: the
/// derivation node sees the correct L2 block.
#[tokio::test]
async fn consecutive_failures_then_success_derives_correctly() {
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

    // Fail the next three attempts. The driver retries on each failure until
    // the fourth send_async call succeeds and the frame lands in pending.
    batcher.fail_next_n_submissions(3);
    batcher.encode_only().await;

    // Poll until the frame has made it through all three failures and back to
    // pending via the successful fourth submission.
    batcher.wait_until_requeued(1).await;

    let block_num = batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "frame must survive three consecutive failures and derive one L2 block");
    assert_eq!(node.l2_safe_number(), 1, "safe head must reach 1 after three retries");
    assert!(block_num >= 1, "frame was eventually mined into an L1 block");
}
