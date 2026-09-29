//! Action tests for span-batch derivation before and after Holocene.

use alloy_primitives::B256;
use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::BaseBlock;
use base_common_genesis::UpgradeConfig;

/// Shared setup helpers for Holocene span-batch action tests.
#[derive(Debug)]
struct HoloceneSpanFixture;

impl HoloceneSpanFixture {
    /// Returns a calldata batcher configuration for deterministic action tests.
    fn batcher_config() -> BatcherConfig {
        BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..BatcherConfig::default()
        }
    }

    /// Returns a pre-Holocene harness for protocol-level Span fixtures.
    fn pre_holocene_harness(batcher: &BatcherConfig) -> ActionTestHarness {
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(batcher).through_granite().build();
        ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg)
    }

    /// Returns a harness with Holocene active from genesis.
    fn post_holocene_harness(batcher: &BatcherConfig) -> ActionTestHarness {
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(batcher).through_holocene().build();
        ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg)
    }
}

fn submit_span_fixture(
    harness: &mut ActionTestHarness,
    chain: &SharedL1Chain,
    config: &BatcherConfig,
    blocks: &[BaseBlock],
    nonce: u64,
) {
    harness
        .submit_span_batch_brotli_calldata(config, blocks, nonce)
        .expect("span fixture submission");
    chain.push(harness.l1.tip().clone());
}

/// Post-Holocene span derivation accepts a valid multi-block span batch.
#[tokio::test]
async fn post_holocene_multi_block_span_derives() {
    const BLOCK_COUNT: u64 = 3;

    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &blocks, 0);

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, BLOCK_COUNT as usize, "all span blocks should derive");
    assert_eq!(node.l2_safe_number(), BLOCK_COUNT, "safe head should reach the final span block");
}

/// Post-Holocene span derivation accepts a valid span batch crossing an L1 epoch boundary.
#[tokio::test]
async fn post_holocene_span_crossing_l1_epoch_boundary_derives() {
    const BLOCK_COUNT: u64 = 6;

    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    harness.mine_l1_blocks(1);
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;
    assert_eq!(sequencer.head().l1_origin.number, 1, "last block must reference L1 block 1");

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &blocks, 0);

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, BLOCK_COUNT as usize, "all cross-epoch span blocks should derive");
    assert_eq!(node.l2_safe_number(), BLOCK_COUNT, "safe head should cross the L1 epoch boundary");
}

/// Post-Holocene span derivation rejects a span whose L1 origin hash points to
/// an orphaned L1 fork.
#[tokio::test]
async fn post_holocene_stale_span_l1_origin_check_after_reorg_is_rejected() {
    const BLOCK_COUNT: u64 = 6;

    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    harness.mine_l1_blocks(1);
    let old_l1_1_hash = harness.l1.block_by_number(1).expect("old L1 block 1").hash();
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer.build_next_blocks_with_single_transactions(BLOCK_COUNT).await;
    assert_eq!(sequencer.head().l1_origin.number, 1, "stale span must reference L1 block 1");

    harness.l1.reorg_to(0).expect("reorg to genesis");
    harness.l1.mine_block();
    let new_l1_1_hash = harness.l1.block_by_number(1).expect("new L1 block 1").hash();
    assert_ne!(old_l1_1_hash, new_l1_1_hash, "replacement L1 block must have a new hash");

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &blocks, 0);

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 0, "stale span origin check must be rejected");
    assert_eq!(node.l2_safe_number(), 0, "stale span must not advance safe head");
}

/// Pre-Holocene `BatchQueue` buffers a future span batch and derives it after the gap is filled.
#[tokio::test]
async fn pre_holocene_future_span_is_buffered_by_batch_queue() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::pre_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    submit_span_fixture(&mut harness, &chain, &batcher_cfg, std::slice::from_ref(&block_2), 0);
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future pre-Holocene span should wait for the missing parent");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &[block_1], 100);
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "pre-Holocene BatchQueue should derive the new and buffered spans");
    assert_eq!(node.l2_safe_number(), 2, "safe head should include the buffered future span");
}

/// Post-Holocene strict ordering drops a future span batch instead of buffering it.
#[tokio::test]
async fn post_holocene_future_span_is_dropped_not_buffered() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    submit_span_fixture(&mut harness, &chain, &batcher_cfg, std::slice::from_ref(&block_2), 0);
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future post-Holocene span should be dropped");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &[block_1], 100);
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "only the expected next span should derive");
    assert_eq!(node.l2_safe_number(), 1, "future span must not have been buffered");

    submit_span_fixture(&mut harness, &chain, &batcher_cfg, &[block_2], 200);
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "resubmitted span should derive after its parent is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance after resubmission");
}

/// Post-Holocene strict ordering also drops future singular batches instead of buffering them.
#[tokio::test]
async fn post_holocene_future_singular_is_dropped_not_buffered() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    node.initialize().await;

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "future post-Holocene singular batch should be dropped");
    assert_eq!(node.l2_safe_number(), 0, "safe head should remain at genesis");

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "only the expected next singular batch should derive");
    assert_eq!(node.l2_safe_number(), 1, "future singular batch must not have been buffered");

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_2]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "resubmitted singular batch should derive after its parent is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance after resubmission");
}

/// Post-Holocene past singular batches are ignored without flushing following batches.
#[tokio::test]
async fn post_holocene_past_singular_does_not_flush_channel() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::post_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1.clone()]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "block 1 should derive before the stale replay");
    assert_eq!(node.l2_safe_number(), 1);

    harness.submit_l2_blocks(&chain, batcher_cfg, vec![block_1, block_2]).await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "stale block 1 should be ignored and block 2 should still derive");
    assert_eq!(node.l2_safe_number(), 2, "post-Holocene past batches must not flush the channel");
}

/// Pre-Holocene past singular replays are ignored without poisoning following batches.
#[tokio::test]
async fn pre_holocene_past_singular_does_not_poison_channel() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    let mut harness = HoloceneSpanFixture::pre_holocene_harness(&batcher_cfg);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
    let block_1 = blocks.remove(0);
    let block_2 = blocks.remove(0);

    let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1.clone()]).await;

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "block 1 should derive before the stale replay");
    assert_eq!(node.l2_safe_number(), 1);

    harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1, block_2.clone()]).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "stale block 1 should be ignored and block 2 should still derive");
    assert_eq!(node.l2_safe_number(), 2);
}

/// Derivation accepts singular and span batches in the same L1 stream, before and after
/// Holocene.
#[tokio::test]
async fn mixed_singular_and_span_batches_derive_before_and_after_holocene() {
    let batcher_cfg = HoloceneSpanFixture::batcher_config();
    for (fork, mut harness) in [
        ("pre-Holocene", HoloceneSpanFixture::pre_holocene_harness(&batcher_cfg)),
        ("post-Holocene", HoloceneSpanFixture::post_holocene_harness(&batcher_cfg)),
    ] {
        let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
        let mut sequencer = harness.create_l2_sequencer(l1_chain);
        let mut blocks = sequencer.build_next_blocks_with_single_transactions(2).await;
        let block_1 = blocks.remove(0);
        let block_2 = blocks.remove(0);

        let (mut node, chain) = harness.create_test_rollup_node_from_sequencer(
            &mut sequencer,
            SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
        );
        harness.submit_l2_blocks(&chain, batcher_cfg.clone(), vec![block_1]).await;
        submit_span_fixture(&mut harness, &chain, &batcher_cfg, &[block_2], 100);

        node.initialize().await;
        let derived = node.run_until_idle().await;

        assert_eq!(derived, 2, "{fork}: singular and span batches should both derive");
        assert_eq!(node.l2_safe_number(), 2, "{fork}: safe head should include both batch formats");
    }
}

/// A span batch covering blocks 1–4 where block 3 is the first Jovian block
/// but contains user transactions, which the upgrade block may not, is
/// partially rejected. The pipeline derives blocks 1–2 from the span batch,
/// then fails on block 3 (`NonEmptyTransitionBlock` → `FlushChannel` under Holocene),
/// dropping the span batch's channel. Blocks 3–4 are never derived from the
/// span batch.
///
/// A single bad block mid-span loses the remaining blocks of the channel, so
/// blocks 3–4 must be submitted again. This is the key difference from singular
/// batches where only the offending block is dropped and all others derive fine
/// (tested in `jovian_non_empty_transition_batch_generates_deposit_only_block`).
///
/// Blocks 3, now empty, and 4 then derive from a corrected span batch in a new
/// channel. `NonEmptyTransitionBlock` only fires for the first Jovian block, not
/// for earlier upgrades like Ecotone or Isthmus.
#[tokio::test]
async fn span_batch_with_non_empty_transition_block_rejected() {
    // All forks through Isthmus active at genesis. Jovian activates at ts=6
    // (L2 block 3 with block_time=2). Because only Jovian is "new" at ts=6,
    // `is_first_jovian_block(6)` returns true and the NonEmptyTransitionBlock
    // check fires for block 3 alone.
    let jovian_time = 6u64;
    let upgrades = UpgradeConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(0),
        jovian_time: Some(jovian_time),
        ..Default::default()
    };
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 4 L2 blocks. build_next_block_with_single_transaction() includes a user transaction in
    // every block. Block 3 (ts=6) is the first Jovian block, which must be
    // deposit-only — including a user tx here is the deliberate error.
    let block1 = builder.build_next_block_with_single_transaction().await; // ts=2
    let block2 = builder.build_next_block_with_single_transaction().await; // ts=4
    let block3_invalid = builder.build_next_block_with_single_transaction().await; // ts=6
    let block4 = builder.build_next_block_with_single_transaction().await; // ts=8

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // --- Phase 1: submit all 4 blocks as one span fixture (block 3 has user txs) ---
    h.submit_span_batch_brotli_calldata(
        &batcher_cfg,
        &[block1.clone(), block2.clone(), block3_invalid, block4.clone()],
        0,
    )
    .expect("span fixture submission");
    chain.push(h.l1.tip().clone()); // L1 block 1: span batch with invalid block 3

    // The sequencer registered state roots for blocks 3 and 4 that will not match
    // what derivation produces (block 3 becomes deposit-only; block 4 has a different
    // parent). Clear the state root entries so the engine skips validation for these.
    node.register_block_hash(3, B256::ZERO);
    node.register_block_hash(4, B256::ZERO);

    node.initialize().await;
    node.run_until_idle().await;

    // Under Holocene, when the pipeline reaches block 3 in the span batch and
    // detects a user tx in the upgrade block, it sends FlushChannel (via
    // BatchStream::flush), discarding the channel entirely. Blocks 1 and 2 were
    // already emitted as individual batches before the failure, so safe head is 2.
    assert_eq!(
        node.l2_safe_number(),
        2,
        "blocks 1 and 2 should derive before span batch fails on block 3"
    );

    // --- Phase 2: resubmit blocks 3–4 with block 3 correctly empty ---
    //
    // The primary builder is now at block 4; build_empty_block() on it would
    // produce block 5 (wrong timestamp). Instead, create a fresh sequencer
    // starting from genesis, advance it to block 2's state, then build the
    // correct recovery blocks 3 (empty, ts=6) and 4 (user tx, ts=8).
    //
    // Rebuilding blocks 1–2 deterministically makes the recovery span's parent check match
    // canonical block 2. `BatchStream` then assigns each span-derived singular the current
    // safe-head hash when emitting it.
    {
        let l1_chain2 = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
        let mut builder2 = h.create_l2_sequencer(l1_chain2);
        let _rb1 = builder2.build_next_block_with_single_transaction().await;
        let _rb2 = builder2.build_next_block_with_single_transaction().await;
        let block3_empty = builder2.build_empty_block().await;
        let block4_recovery = builder2.build_next_block_with_single_transaction().await;

        h.submit_span_batch_brotli_calldata(&batcher_cfg, &[block3_empty, block4_recovery], 100)
            .expect("recovery span fixture submission");
    }
    chain.push(h.l1.tip().clone()); // L1 block 2: recovery span batch (blocks 3–4)

    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 4, "after recovery submission, safe head must reach block 4");
}
