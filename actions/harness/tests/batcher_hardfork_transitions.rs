//! Action tests for batch format transitions across hardfork boundaries.

use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DerivedBlock, L1MinerConfig,
    SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_consensus_genesis::{HardForkConfig, RollupConfig};
use base_protocol::BatchType;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// A. Span batch with non-empty hardfork transition block is rejected
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
/// for earlier hardforks like Ecotone or Isthmus.
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
    let hardforks = HardForkConfig {
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
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
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

    // --- Phase 1: submit all 4 blocks as one span batch (block 3 has user txs) ---
    {
        let span_cfg = BatcherConfig { batch_type: BatchType::Span, ..batcher_cfg.clone() };
        let mut source = ActionL2Source::new();
        source.push(block1.clone());
        source.push(block2.clone());
        source.push(block3_invalid);
        source.push(block4.clone());
        Batcher::new(source, &h.rollup_config, span_cfg).advance(&mut h.l1).await;
    }
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
    // The Holocene BatchValidator overwrites each singular batch's parent_hash
    // with the current chain head before validating, so the recovery blocks
    // only need the correct timestamps and user-tx content — not the exact
    // parent hashes from the primary sequencer's chain.
    {
        let l1_chain2 = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
        let mut builder2 = h.create_l2_sequencer(l1_chain2);
        let _rb1 = builder2.build_next_block_with_single_transaction().await;
        let _rb2 = builder2.build_next_block_with_single_transaction().await;
        let block3_empty = builder2.build_empty_block().await;
        let block4_recovery = builder2.build_next_block_with_single_transaction().await;

        let span_cfg = BatcherConfig { batch_type: BatchType::Span, ..batcher_cfg };
        let mut source = ActionL2Source::new();
        source.push(block3_empty);
        source.push(block4_recovery);
        Batcher::new(source, &h.rollup_config, span_cfg).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 2: recovery span batch (blocks 3–4)

    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 4, "after recovery submission, safe head must reach block 4");
}

// ---------------------------------------------------------------------------
// B. Mixed singular and span batches in the same derivation run
// ---------------------------------------------------------------------------

/// After Fjord (which cascades to activate Delta), the pipeline must accept
/// **both** singular and span batches in the same derivation run. This test
/// submits block 1 as a singular batch in L1 block 1 and block 2 as a span
/// batch in L1 block 2.
///
/// Prior to Delta, span batches are rejected outright (`SpanBatchPreDelta`).
/// After Delta, both formats are valid. The derivation pipeline must derive
/// all 2 L2 blocks regardless of which format each batch uses.
#[tokio::test]
async fn mixed_singular_and_span_batches_after_delta() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    // Fjord cascades: Canyon, Delta, Ecotone, Fjord all active at genesis.
    let hardforks = HardForkConfig { fjord_time: Some(0), ..Default::default() };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: block 1 as a SINGULAR batch.
    {
        let singular_cfg = BatcherConfig { batch_type: BatchType::Single, ..batcher_cfg.clone() };
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, singular_cfg).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: singular batch for L2 block 1

    // L1 block 2: block 2 as a SPAN batch.
    {
        let span_cfg = BatcherConfig { batch_type: BatchType::Span, ..batcher_cfg };
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, span_cfg).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 2: span batch for L2 block 2

    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(
        total_derived, 2,
        "mixed singular + span batches must both derive; safe head should reach 2"
    );
    assert_eq!(
        node.l2_safe_number(),
        2,
        "mixed singular + span batches must both derive; safe head should reach 2"
    );
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
    let hardforks = HardForkConfig {
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
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();

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
    let block_1_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(block_1_num).await;

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
    let late_block_num = h.l1.mine_block().number();
    chain.push(h.l1.tip().clone());
    batcher.confirm_staged(late_block_num).await;

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
// D. Jovian SingleBatch transition block is deposit-only
// ---------------------------------------------------------------------------

/// When a `SingleBatch` is submitted for the first Jovian upgrade block (block 3
/// at ts=6) containing user transactions, derivation drops the batch
/// (`NonEmptyTransitionBlock`) and generates a deposit-only block in its place
/// once the sequencer window expires. Unlike span batches (test A above), only
/// the offending block is dropped — the remaining singular batches for blocks
/// 1, 2, and 4 derive successfully.
///
/// Setup: all forks through Isthmus active at genesis, Jovian at timestamp 6
/// (L2 block 3 with `block_time=2`). Each L2 block is submitted as a separate
/// `SingleBatch` channel. A small `seq_window_size` (4) ensures the sequencer
/// window expires quickly so the pipeline can force-generate the deposit-only
/// block for block 3 without mining thousands of L1 blocks.
///
/// After the pipeline drops block 3's batch and the sequencer window expires,
/// it auto-generates a deposit-only block 3 and then derives block 4 from its
/// submitted batch. Safe head reaches 4.
#[tokio::test]
async fn jovian_single_batch_transition_block_deposit_only() {
    let jovian_time = 6u64;
    let hardforks = HardForkConfig {
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
        batch_type: BatchType::Single,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    // Use a small seq_window_size so the pipeline can force-generate deposit-only
    // blocks without needing to mine thousands of empty L1 blocks.
    //
    // L1 block_time=2 ensures L1 timestamps closely track L2 timestamps and
    // don't create a wide gap that would generate many spurious empty L2 blocks
    // when the sequencer window expires.
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_hardforks(hardforks)
        .with_seq_window_size(4)
        .build();
    let l1_config = L1MinerConfig { block_time: 2 };
    let mut h = ActionTestHarness::new(l1_config, rollup_cfg);

    // The sequencer is initialized with the L1 chain at genesis (before any
    // L1 blocks are mined). As a result every L2 block built by `builder` will
    // have L1 origin = genesis (block 0). This means the sequencer window
    // [0, 0 + seq_window_size) governs *all* four L2 blocks, which is the
    // behaviour the deposit-only assertion relies on.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 4 L2 blocks. build_next_block_with_single_transaction() includes a user transaction in
    // every block. Block 3 (ts=6) is the first Jovian block — including a
    // user tx is the deliberate error that derivation must handle.
    let block1 = builder.build_next_block_with_single_transaction().await; // ts=2
    let block2 = builder.build_next_block_with_single_transaction().await; // ts=4
    let block3_invalid = builder.build_next_block_with_single_transaction().await; // ts=6
    let block4 = builder.build_next_block_with_single_transaction().await; // ts=8

    // Precondition: block 3 must contain at least one user transaction (deposits
    // don't count). If build_next_block_with_single_transaction() ever started returning empty blocks,
    // this test would silently stop exercising the NonEmptyTransitionBlock path.
    assert!(
        block3_invalid.body.transactions.len() > 1,
        "block 3 must have deposit tx + at least one user tx to trigger NonEmptyTransitionBlock; \
         got {} txs",
        block3_invalid.body.transactions.len(),
    );

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Submit each block as a separate SingleBatch channel, one L1 block each.
    // L1 blocks 1–4 each contain one singular batch.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in [block1, block2, block3_invalid, block4] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    // Mine additional empty L1 blocks so the sequencer window (size=4) expires
    // for block 3's epoch. The sequencer is initialized with only L1 genesis in
    // its view, so all L2 blocks have L1 origin = genesis (epoch 0). The window
    // expires at L1 block 0 + 4 = 4; the pipeline generates the deposit-only block
    // when processing L1 block 5 (the first block past the window). We already
    // have L1 blocks 1–4 from batch submission; mine 2 more (blocks 5-6) to
    // ensure the window expires before the pipeline tip.
    for _ in 0..2 {
        h.mine_and_push(&chain);
    }

    // The sequencer registered state roots for blocks 3 and 4 that will not match
    // what derivation produces (block 3 becomes deposit-only; block 4 has a different
    // parent). Clear the state root entries so the engine skips validation for these.
    node.register_block_hash(3, B256::ZERO);
    node.register_block_hash(4, B256::ZERO);

    node.initialize().await;

    // Signal all L1 blocks to the node and drive derivation.
    let tip = h.l1.latest_number();
    for _ in 1..=tip {
        node.run_until_idle().await;
    }

    // With singular batches and the Holocene batch validator: the pipeline
    // derives blocks 1 and 2 from their submitted batches; drops block 3's
    // batch (NonEmptyTransitionBlock for the first Jovian block containing
    // user txs); force-generates a deposit-only block 3 once the sequencer
    // window expires; then derives block 4 from its submitted batch.
    assert_eq!(
        node.l2_safe_number(),
        4,
        "singular batches: safe head must reach 4 (block 3 replaced with deposit-only)"
    );

    // Verify that block 3 is genuinely deposit-only — not that the pipeline
    // accepted the invalid batch and derived a normal block. Without this
    // assertion, removing the NonEmptyTransitionBlock validation would still
    // produce safe_head == 4.
    let block3: DerivedBlock =
        node.derived_block(3).expect("block 3 must have been derived before safe head reached 4");
    assert!(
        block3.is_deposit_only(),
        "block 3 must be deposit-only (NonEmptyTransitionBlock batch dropped); \
         got {} user txs",
        block3.user_tx_count,
    );
}
