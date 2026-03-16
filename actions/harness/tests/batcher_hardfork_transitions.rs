#![doc = "Action tests for batch format transitions across hardfork boundaries."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatchType, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};
use base_consensus_genesis::HardForkConfig;

// ---------------------------------------------------------------------------
// A. Span batch with non-empty hardfork transition block is rejected
//
// op-e2e ref: TestHardforkMiddleOfSpanBatch
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

    // Build 4 L2 blocks. build_next_block() includes a user transaction in
    // every block. Block 3 (ts=6) is the first Jovian block, which must be
    // deposit-only — including a user tx here is the deliberate error.
    let block1 = builder.build_next_block().expect("build L2 block 1"); // ts=2
    let block2 = builder.build_next_block().expect("build L2 block 2"); // ts=4
    let block3_invalid = builder.build_next_block().expect("build L2 block 3 (invalid)"); // ts=6
    let block4 = builder.build_next_block().expect("build L2 block 4"); // ts=8

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &builder,
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
        Batcher::new(source, &h.rollup_config, span_cfg)
            .advance(&mut h.l1)
            .await
            .expect("encode span batch with invalid block 3");
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: span batch with invalid block 3

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    verifier.act_l2_pipeline_full().await.expect("pipeline after block 1");

    // Under Holocene, when the pipeline reaches block 3 in the span batch and
    // detects a user tx in the upgrade block, it sends FlushChannel (via
    // BatchStream::flush), discarding the channel entirely. Blocks 1 and 2 were
    // already emitted as individual batches before the failure, so safe head is 2.
    assert_eq!(
        verifier.l2_safe().block_info.number,
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
        let _rb1 = builder2.build_next_block().expect("advance recovery builder past block 1");
        let _rb2 = builder2.build_next_block().expect("advance recovery builder past block 2");
        let block3_empty = builder2.build_empty_block().expect("build empty block 3 for recovery");
        let block4_recovery = builder2.build_next_block().expect("build recovery block 4");

        let span_cfg = BatcherConfig { batch_type: BatchType::Span, ..batcher_cfg };
        let mut source = ActionL2Source::new();
        source.push(block3_empty);
        source.push(block4_recovery);
        Batcher::new(source, &h.rollup_config, span_cfg)
            .advance(&mut h.l1)
            .await
            .expect("encode recovery span batch");
    }
    chain.push(h.l1.tip().clone()); // L1 block 2: recovery span batch (blocks 3–4)

    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal block 2");
    verifier.act_l2_pipeline_full().await.expect("pipeline after block 2");

    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "after recovery submission, safe head must reach block 4"
    );
}

// ---------------------------------------------------------------------------
// B. Mixed singular and span batches in the same derivation run
//
// op-e2e ref: TestMixOfBatchesAfterHardfork
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

    let block1 = builder.build_next_block().expect("build L2 block 1");
    let block2 = builder.build_next_block().expect("build L2 block 2");

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: block 1 as a SINGULAR batch.
    {
        let singular_cfg = BatcherConfig { batch_type: BatchType::Single, ..batcher_cfg.clone() };
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, singular_cfg)
            .advance(&mut h.l1)
            .await
            .expect("encode singular batch");
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: singular batch for L2 block 1

    // L1 block 2: block 2 as a SPAN batch.
    {
        let span_cfg = BatcherConfig { batch_type: BatchType::Span, ..batcher_cfg };
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, span_cfg)
            .advance(&mut h.l1)
            .await
            .expect("encode span batch");
    }
    chain.push(h.l1.tip().clone()); // L1 block 2: span batch for L2 block 2

    verifier.initialize().await.expect("initialize");

    // Drive derivation L1 block by block. The first batch (singular) derives
    // L2 block 1; the second (span) derives L2 block 2.
    for i in 1..=2u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("pipeline");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "mixed singular + span batches must both derive; safe head should reach 2"
    );
}
