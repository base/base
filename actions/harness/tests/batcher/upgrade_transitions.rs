//! Action tests for batch format transitions across upgrade boundaries.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoding_channel::{DaType, EncoderConfig};
use base_common_chain_config::{BaseUpgradeConfig, RollupConfig, UpgradeConfig};

// ---------------------------------------------------------------------------
// A. Span batch with non-empty upgrade transition block is rejected
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// B. Mixed singular and span batches in the same derivation run
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// C. Span batches stop at Cobalt and recover through production SingleBatch
// ---------------------------------------------------------------------------

/// Unsupported span channels do not advance the safe head. Production singular
/// batches subsequently derive every block across Cobalt's 200ms cadence.
#[tokio::test]
async fn span_batch_is_rejected_and_singular_batches_derive_across_cobalt() {
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
            cobalt: Some(6),
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

    h.submit_unsupported_span_calldata(&batcher_cfg, 100).expect("span fixture submission");
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    node.run_until_idle().await;
    assert_eq!(node.l2_safe_number(), 0, "span batches must be rejected even before Cobalt");

    let mut source = ActionL2Source::new();
    for block in blocks {
        source.push(block);
    }
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let recovered = node.run_until_idle().await;
    assert_eq!(recovered, 8, "singular batches must derive on both sides of Cobalt");
    assert_eq!(node.l2_safe_number(), 8, "safe head must advance across Cobalt");
}

// ---------------------------------------------------------------------------
// D. Granite channel timeout enforcement
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
/// Phase 1: encode one L2 block into a multi-frame channel (`max_frame_size=80`),
/// submit only frame 0 in L1 block 1, then mine 51 more empty L1 blocks.
/// The channel's `open_block_number` is 1 and `1 + 50 = 51 < 52`, so the
/// channel is timed out by the time the pipeline reaches L1 block 52.
///
/// Phase 2 (recovery): a new batcher submits all frames in a single L1 block
/// and derivation advances the safe head to 1.
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

    // Verify the config has the expected timeout values.
    assert_eq!(
        rollup_cfg.granite_channel_timeout,
        RollupConfig::GRANITE_CHANNEL_TIMEOUT,
        "granite_channel_timeout must be {}",
        RollupConfig::GRANITE_CHANNEL_TIMEOUT
    );
    assert_eq!(
        rollup_cfg.channel_timeout, 300,
        "pre-Granite channel_timeout must be 300 (Base mainnet default)"
    );

    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode block into multiple frames (max_frame_size=80 forces multi-frame).
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
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
    batcher.confirm_staged(h.l1.tip()).await;

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
    batcher.confirm_staged(h.l1.tip()).await;

    let derived = node.run_until_idle().await;
    assert_eq!(
        derived, 0,
        "late non-zero frames after timeout create an incomplete channel; no L2 block derived"
    );
    assert_eq!(
        node.l2_safe_number(),
        0,
        "safe head must still be at genesis before recovery; channel timed out"
    );

    // --- Recovery: new batcher, all frames in one L1 block ---
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    Batcher::new(source2, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    let recovered = node.run_until_idle().await;

    assert_eq!(recovered, 1, "recovery channel should derive L2 block 1");
    assert_eq!(node.l2_safe_number(), 1, "safe head should recover to 1");
}

// ---------------------------------------------------------------------------
// E. Jovian SingleBatch transition block is deposit-only
// ---------------------------------------------------------------------------
