#![doc = "TDD action test skeletons for channel timeout and interleaving scenarios."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder, block_info_from,
};

// ---------------------------------------------------------------------------
// A. Channel timeout — first frame's inclusion span exceeds channel_timeout
// ---------------------------------------------------------------------------

/// When a channel's frames are spread across L1 blocks separated by more than
/// `channel_timeout` blocks, the derivation pipeline discards the entire
/// channel. The batcher must detect this and resubmit the affected L2 blocks
/// in a new channel.
///
/// ## Setup
///
/// - `channel_timeout = 2` (very tight: channel expires if span > 2 blocks)
/// - Force a multi-frame channel by setting `max_frame_size = 80`
/// - Build L2 blocks and encode them into a multi-frame channel
/// - Submit frame 0 in L1 block N
/// - Advance L1 by `channel_timeout + 1` empty blocks (no remaining frames)
/// - Submit remaining frames — they arrive too late
///
/// ## Expected behaviour
///
/// The derivation pipeline:
/// 1. Receives frame 0 in L1 block N, opens the channel
/// 2. After `channel_timeout` L1 blocks pass without the channel completing,
///    the channel is pruned from the channel bank
/// 3. Remaining frames arrive but the channel ID is unknown → silently dropped
/// 4. Safe head does NOT advance for the timed-out channel's L2 blocks
///
/// ## Recovery
///
/// After the timeout, the batcher resubmits all L2 blocks in a fresh channel
/// (new `ChannelDriver` instance = new channel ID) within the timeout window.
/// The pipeline derives the L2 blocks from the new channel.
///
/// ## Harness requirements
///
/// This test uses `Batcher::encode_frames()` + `Batcher::submit_frames()`
/// (selective frame submission) which ALREADY EXIST. No new harness methods
/// are needed for the basic scenario.
///
/// NOTE: The `ChannelDriver` currently uses `ChannelId::default()` ([0u8; 16])
/// for every flush. When the batcher resubmits in a "new" channel, it will
/// have the same channel ID as the timed-out one. This is acceptable if the
/// channel bank has already pruned the old channel (the new frames start at
/// frame 0 again, so the bank treats it as a new channel). If this causes
/// issues, `ChannelDriver` needs a `with_channel_id(id)` builder or random
/// ID generation.
#[tokio::test]
async fn channel_timeout_triggers_channel_invalidation() {
    use base_batcher_encoder::EncoderConfig;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_channel_timeout(2).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build 1 L2 block and encode it into multiple frames.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build L2 block 1");

    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().expect("encode multi-frame channel");
    assert!(
        frames.len() >= 2,
        "expected multi-frame channel with max_frame_size=80, got {} frames",
        frames.len()
    );

    // Submit ONLY frame 0 in L1 block 1.
    batcher.submit_frames(&frames[..1]);
    batcher.flush(&mut h.l1);

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.mine_and_push(&chain); // L1 block 1: frame 0 only

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("step block 1");

    // Nothing derived yet — channel incomplete (only frame 0).
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "incomplete channel should not advance safe head"
    );

    // Mine `channel_timeout + 1 = 3` empty L1 blocks to expire the channel.
    // L1 blocks 2, 3, 4 are empty.
    for _ in 0..3 {
        h.mine_and_push(&chain);
    }

    // Signal empty blocks to the pipeline.
    for i in 2..=4 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal empty block");
        verifier.act_l2_pipeline_full().await.expect("step empty block");
    }

    // The channel should now be timed out. Submit the remaining frames —
    // they should be silently ignored.
    {
        let empty_source = ActionL2Source::new();
        let mut late_batcher = h.create_batcher(empty_source, batcher_cfg.clone());
        late_batcher.submit_frames(&frames[1..]);
        late_batcher.flush(&mut h.l1);
    }
    h.mine_and_push(&chain); // L1 block 5: late frames

    let l1_block_5 = block_info_from(h.l1.block_by_number(5).expect("block 5"));
    verifier.act_l1_head_signal(l1_block_5).await.expect("signal block 5");
    let derived = verifier.act_l2_pipeline_full().await.expect("step block 5");
    assert_eq!(derived, 0, "late frames after channel timeout must be ignored");

    // --- Recovery: resubmit in a new channel ---
    // Create a fresh batcher (new ChannelDriver = new channel) and resubmit.
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    let mut batcher2 = h.create_batcher(source2, batcher_cfg);
    batcher2.advance().expect("resubmit in new channel");
    batcher2.flush(&mut h.l1);
    h.mine_and_push(&chain); // L1 block 6: fresh channel, all frames

    let l1_block_6 = block_info_from(h.l1.block_by_number(6).expect("block 6"));
    verifier.act_l1_head_signal(l1_block_6).await.expect("signal block 6");
    let recovered = verifier.act_l2_pipeline_full().await.expect("step block 6");

    assert_eq!(recovered, 1, "resubmitted channel should derive L2 block 1");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should recover to 1");
}

// ---------------------------------------------------------------------------
// B. Channel timeout with recovery
// ---------------------------------------------------------------------------

/// After a channel times out, the batcher creates a fresh channel containing
/// the same L2 blocks and submits it within the timeout window. The pipeline
/// derives the blocks from the recovery channel.
///
/// This is a simpler variant of [`channel_timeout_triggers_channel_invalidation`]
/// that focuses purely on the recovery path. The timeout is induced the same
/// way as in that test: encode a multi-frame channel, submit only frame 0 in
/// L1 block 1, then let the channel expire over `channel_timeout + 1` empty
/// blocks. The recovery submits all frames in a single L1 block so the new
/// channel completes immediately.
#[tokio::test]
async fn channel_timeout_recovery_resubmits_successfully() {
    use base_batcher_encoder::EncoderConfig;

    // Small max_frame_size forces a multi-frame channel so we can hold back
    // frames to induce a timeout, matching the setup in
    // channel_timeout_triggers_channel_invalidation.
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_channel_timeout(2).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build block 1");

    // Encode into a multi-frame channel.
    let mut source = ActionL2Source::new();
    source.push(block.clone());
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().expect("encode frames");
    assert!(
        frames.len() >= 2,
        "expected multi-frame channel with max_frame_size=80, got {} frames",
        frames.len()
    );

    // Submit only frame 0 in L1 block 1 — channel stays incomplete.
    batcher.submit_frames(&frames[..1]);
    batcher.flush(&mut h.l1);

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.mine_and_push(&chain); // L1 block 1: frame 0 only

    verifier.initialize().await.expect("initialize");

    // Mine channel_timeout + 1 = 3 empty blocks to expire the channel.
    for _ in 0..3 {
        h.mine_and_push(&chain); // L1 blocks 2, 3, 4
    }

    // Step the pipeline through all L1 blocks. The channel is pruned once
    // L1 block 3 is processed (3 − 1 = 2 ≥ channel_timeout = 2).
    for i in 1..=h.l1.latest_number() {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    // The channel timed out — safe head is still at genesis.
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "channel should have timed out; safe head must remain at genesis"
    );

    // Recovery: submit all frames in one L1 block so the new channel
    // completes immediately within the timeout window.
    let mut source2 = ActionL2Source::new();
    source2.push(block);
    let mut batcher2 = h.create_batcher(source2, batcher_cfg);
    batcher2.advance().expect("recovery submit");
    batcher2.flush(&mut h.l1);
    h.mine_and_push(&chain); // L1 block 5: fresh channel, all frames

    let recovery_blk =
        block_info_from(h.l1.block_by_number(h.l1.latest_number()).expect("recovery block"));
    verifier.act_l1_head_signal(recovery_blk).await.expect("signal recovery");
    let recovered = verifier.act_l2_pipeline_full().await.expect("step recovery");

    assert_eq!(recovered, 1, "recovery channel should derive L2 block 1");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should recover to 1");
}

// ---------------------------------------------------------------------------
// C. Channel interleaving — frames from two channels interleaved in L1
// ---------------------------------------------------------------------------

/// Frames from two different channels are submitted to L1 in interleaved
/// order (A0, B0, A1, B1). The derivation pipeline's channel bank must
/// correctly track both channels simultaneously and reassemble them
/// independently.
///
/// ## Setup
///
/// - Build 2 sets of L2 blocks, each encoded into a separate multi-frame
///   channel (using small `max_frame_size` to force multi-frame output)
/// - Submit frames in alternating order across L1 transactions within the
///   same L1 block
///
/// ## Expected behaviour
///
/// Both channels are correctly reassembled and all L2 blocks are derived
/// in the correct order (channel A's blocks first, then channel B's).
///
/// ## Harness requirements
///
/// This test requires distinct channel IDs for the two channels. Currently
/// `ChannelDriver` always uses `ChannelId::default()` ([0u8; 16]). Two
/// options:
///
/// 1. **Add `ChannelDriverConfig::channel_id`** — allow tests to specify
///    the channel ID explicitly:
///    ```rust
///    pub struct ChannelDriverConfig {
///        pub max_frame_size: usize,
///        pub channel_id: Option<ChannelId>,
///    }
///    ```
///
/// 2. **Randomize by default** — have `ChannelDriver::flush()` generate a
///    random `ChannelId` per flush call (matches op-batcher behaviour).
///
/// ## Note on same-block placement
///
/// For interleaving to work across two channels both channels' frames must
/// land in the same L1 block (as separate transactions).  All interleaved
/// frames are submitted to the harness before a single `mine_and_push` call,
/// so they all appear in L1 block 1.
#[tokio::test]
async fn interleaved_channels_correctly_reassembled() {
    use base_batcher_encoder::EncoderConfig;
    let batcher_cfg = BatcherConfig {
        // Small max_frame_size forces each block's batch data to spill across multiple frames,
        // producing distinct channel IDs per encoder instance (BatchEncoder randomizes per channel).
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks — one for each channel.
    let block_a = sequencer.build_next_block().expect("build block A");
    let block_b = sequencer.build_next_block().expect("build block B");

    // Encode channel A (L2 block 1).
    let mut source_a = ActionL2Source::new();
    source_a.push(block_a);
    let mut batcher_a = h.create_batcher(source_a, batcher_cfg.clone());
    let frames_a = batcher_a.encode_frames().expect("encode channel A");
    assert!(frames_a.len() >= 2, "channel A should have 2+ frames, got {}", frames_a.len());

    // Encode channel B (L2 block 2).
    let mut source_b = ActionL2Source::new();
    source_b.push(block_b);
    let mut batcher_b = h.create_batcher(source_b, batcher_cfg.clone());
    let frames_b = batcher_b.encode_frames().expect("encode channel B");
    assert!(frames_b.len() >= 2, "channel B should have 2+ frames, got {}", frames_b.len());

    // Verify channels have distinct IDs (will fail until ChannelDriver is updated).
    assert_ne!(
        frames_a[0].id, frames_b[0].id,
        "channels A and B must have distinct IDs for interleaving"
    );

    // Submit frames interleaved: A0, B0, A1, B1, ...
    // All frames go into the same L1 block (separate txs within the block).
    let max_len = frames_a.len().max(frames_b.len());
    {
        let empty_source = ActionL2Source::new();
        let mut submitter = h.create_batcher(empty_source, batcher_cfg);
        for i in 0..max_len {
            if i < frames_a.len() {
                submitter.submit_frames(&frames_a[i..i + 1]);
            }
            if i < frames_b.len() {
                submitter.submit_frames(&frames_b[i..i + 1]);
            }
        }
        submitter.flush(&mut h.l1);
    }

    // Mine one L1 block containing all interleaved frames.
    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await.expect("initialize");

    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step block 1");

    // Both channels should be reassembled and both L2 blocks derived.
    assert_eq!(derived, 2, "expected 2 L2 blocks derived from interleaved channels");
    assert_eq!(verifier.l2_safe().block_info.number, 2);
}

// ---------------------------------------------------------------------------
// D. Multi-block channel — frames split across consecutive L1 blocks
// ---------------------------------------------------------------------------

/// A single channel whose frames are spread across two consecutive L1 blocks
/// is correctly reassembled by the derivation pipeline.
///
/// This is the primary regression test for the `ProvideBlock` signal fix in
/// [`ChannelBank`]: when a new L1 block arrives via `ProvideBlock`, the channel
/// bank must **not** discard in-progress channels.  Without the fix, frame 0
/// (submitted in L1 block 1) would be lost when L1 block 2 is signalled, and
/// the channel could never complete.
///
/// ## Setup
///
/// - `max_frame_size = 80` forces a multi-frame channel so we can split frame 0
///   and the remainder across two distinct L1 blocks.
/// - Frame 0 is submitted in L1 block 1; remaining frames in L1 block 2.
/// - Both blocks are within the default `channel_timeout` window.
///
/// ## Expected behaviour
///
/// After processing L1 block 2 the pipeline assembles the complete channel and
/// derives L2 block 1.  The safe head advances to 1.
#[tokio::test]
async fn multi_block_channel_assembles_across_l1_blocks() {
    use base_batcher_encoder::EncoderConfig;
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build L2 block 1");

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().expect("encode");
    assert!(
        frames.len() >= 2,
        "need at least 2 frames for this test; got {} (increase payload or decrease max_frame_size)",
        frames.len()
    );

    // Submit frame 0 only → L1 block 1.
    batcher.submit_frames(&frames[..1]);
    batcher.flush(&mut h.l1);

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    h.mine_and_push(&chain); // L1 block 1: frame 0

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("step block 1");

    // Channel is open but incomplete — safe head stays at genesis.
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "channel incomplete after block 1; safe head must stay at genesis"
    );

    // Submit remaining frames → L1 block 2 (well within channel_timeout).
    {
        let empty_source = ActionL2Source::new();
        let mut batcher2 = h.create_batcher(empty_source, batcher_cfg);
        batcher2.submit_frames(&frames[1..]);
        batcher2.flush(&mut h.l1);
    }
    h.mine_and_push(&chain); // L1 block 2: remaining frames

    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal block 2");
    let derived = verifier.act_l2_pipeline_full().await.expect("step block 2");

    assert_eq!(derived, 1, "multi-block channel must yield 1 L2 block");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head must advance to 1");
}
