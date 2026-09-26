//! Action tests for batch format transitions across upgrade boundaries.

use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// A. Span batch with non-empty upgrade transition block is rejected
// ---------------------------------------------------------------------------

/// A span batch covering blocks 1–4 where block 3 is the first Jovian block
/// but **contains user transactions** (which is illegal for the upgrade block)
/// is partially rejected. The pipeline derives blocks 1–2 from the span batch,
/// then fails on block 3 (`NonEmptyTransitionBlock` → `FlushChannel` under Holocene),
/// dropping the span batch's channel. Blocks 3–4 are never derived from the
/// span batch.
///
/// This demonstrates the **all-or-nothing** failure mode for span batches: a
/// single bad block mid-span loses the remaining blocks in the channel, forcing
/// a re-submission of blocks 3–4. This is the key difference from singular
/// batches where only the offending block is dropped and all others derive fine
/// (tested in `jovian_non_empty_transition_batch_generates_deposit_only_block`).
///
/// Recovery: blocks 3 (empty) and 4 are resubmitted as a corrected span batch
/// in a new channel; safe head advances to 4.
///
/// Note: `NonEmptyTransitionBlock` only fires for the first Jovian block, not
/// for earlier upgrades like Ecotone or Isthmus.
#[tokio::test]
async fn span_batch_with_non_empty_transition_block_rejected() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("error"))
        .with_test_writer()
        .try_init();
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

// ---------------------------------------------------------------------------
// B. Span batches stop at Denim and recover through production SingleBatch
// ---------------------------------------------------------------------------

/// A historical Span fixture may derive its pre-Denim prefix, but the cached
/// tail must be discarded before the first Denim block. Resubmitting that tail
/// through the production `SingleBatch` batcher must derive across Denim's 200ms
/// block cadence.
#[tokio::test]
async fn span_batch_stops_at_denim_and_recovers_with_single_batches() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let upgrades = UpgradeConfig {
        regolith_time: Some(0),
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(0),
        jovian_time: Some(0),
        base: BaseUpgradeConfig {
            azul: Some(0),
            beryl: Some(0),
            cobalt: Some(0),
            denim: Some(6),
            ..Default::default()
        },
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    assert_eq!(
        (3..=8).map(|number| rollup_cfg.l2_block_timestamp_parts(number)).collect::<Vec<_>>(),
        vec![(6, 0), (6, 200), (6, 400), (6, 600), (6, 800), (7, 0)]
    );
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut blocks = Vec::new();
    for _ in 0..8 {
        blocks.push(builder.build_next_block_with_single_transaction().await);
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.submit_span_batch_brotli_calldata(&batcher_cfg, &blocks, 100)
        .expect("span fixture submission");
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        2,
        "only blocks before Denim activation at block 3 may derive from the span"
    );

    let mut source = ActionL2Source::new();
    for block in blocks.into_iter().skip(2) {
        source.push(block);
    }
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let recovered = node.run_until_idle().await;
    assert_eq!(recovered, 6, "the SingleBatch path must recover all post-Denim blocks");
    assert_eq!(node.l2_safe_number(), 8, "safe head must advance across Denim");
}

// ---------------------------------------------------------------------------
// C. Granite channel timeout enforcement
//
// Verifies that the post-Granite 50-block channel timeout is enforced.
// ---------------------------------------------------------------------------

/// After Granite activates, the channel timeout drops from 300 to 50 blocks.
/// A channel whose first frame is included in L1 block 1 must time out when
/// 51 or more additional L1 blocks pass without the channel being completed
/// (i.e., origin.number > `open_block` + 50).
///
/// Setup: All forks through Fjord active at genesis; Granite activates at
/// timestamp 6 (L2 block 3 with `block_time=2`). Because the default L1
/// `block_time` is 12 seconds, L1 block 1's timestamp is 12 — well past
/// Granite activation — so the 50-block timeout applies from the first L1
/// block that contains batch data.
///
/// Encode one L2 block into a multi-frame channel (`max_frame_size=80`), submit only frame 0
/// in L1 block 1, then mine 51 more empty L1 blocks. The channel's `open_block_number` is 1
/// and `1 + 50 = 51 < 52`, so the channel is timed out by the time the pipeline reaches L1
/// block 52. How the batcher recovers from that is in `recovery.rs`.
#[tokio::test]
async fn granite_channel_timeout_enforced() {
    // All forks through Fjord at genesis, Granite at timestamp 6.
    // The pre-Granite channel_timeout (300) is never exercised because every
    // L1 origin processed by the pipeline has timestamp >= 12 > 6.
    let upgrades = UpgradeConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(6),
        ..Default::default()
    };
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();

    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode block into multiple frames (max_frame_size=80 forces multi-frame).
    let batcher = Batcher::new(ActionL2Source::from_blocks([block]), &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;

    let frame_count = batcher.pending_count();
    assert!(
        frame_count >= 2,
        "expected multi-frame channel with max_frame_size=80, got {frame_count} frames",
    );

    // L1 block 1: submit only frame 0. Channel opens at block 1.
    batcher.stage_n_frames(&mut h.l1, 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 0, "incomplete channel should not advance safe head");

    // Mine 51 empty L1 blocks (blocks 2..=52). After block 52, the channel
    // opened at block 1 has been open for 51 blocks: 1 + 50 = 51 < 52,
    // triggering timeout under Granite's 50-block limit.
    for _ in 0..51 {
        h.mine_and_push(&chain);
    }

    // Signal all empty blocks to the verifier.
    for _ in 2..=52u64 {
        node.run_until_idle().await;
    }

    assert_eq!(
        node.l2_safe_number(),
        0,
        "channel must have timed out under Granite's 50-block limit; safe head stays at 0"
    );

    // Submit remaining frames — they arrive after the channel timed out.
    // ChannelBank silently drops frames for timed-out channels (lazy eviction:
    // the timeout check fires in read() before any frame is delivered). If the
    // timed-out entry was already flushed, the late frames would create a new
    // incomplete channel starting from a non-zero frame number, which can also
    // never become ready. Either way, no L2 block is derived.
    batcher.stage_n_frames(&mut h.l1, frame_count - 1);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    batcher.observe_l1_block(h.l1.tip()).await;

    let derived = node.run_until_idle().await;
    assert_eq!(
        derived, 0,
        "late non-zero frames after timeout create an incomplete channel; no L2 block derived"
    );
    assert_eq!(node.l2_safe_number(), 0, "the channel timed out");
}
