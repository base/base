#![doc = "Action tests for the Ecotone hardfork activation boundary."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::HardForkConfig;
use base_protocol::L1BlockInfoTx;

/// Decode the [`L1BlockInfoTx`] from the first (deposit) transaction of an [`OpBlock`].
fn l1_info_from_block(block: &OpBlock) -> L1BlockInfoTx {
    let OpTxEnvelope::Deposit(sealed) = &block.body.transactions[0] else {
        panic!("first transaction must be a deposit");
    };
    L1BlockInfoTx::decode_calldata(sealed.inner().input.as_ref())
        .expect("L1 info calldata must decode")
}

// ---------------------------------------------------------------------------
// A. L1 info format transitions at Ecotone activation
//
// op-e2e ref: ecotone_fork_test.go
// ---------------------------------------------------------------------------

/// The L1 info deposit transaction changes format at Ecotone activation in
/// **two steps**:
///
/// 1. Before Ecotone: `L1BlockInfoTx::Bedrock` (no blob base fee, 4-byte
///    `setL1BlockValues` selector).
/// 2. At the **first** Ecotone block: still `Bedrock` format, because the
///    `L1Block` contract has not yet been upgraded (upgrade transactions are
///    placed *after* the L1 info deposit, so the contract is still on the old
///    ABI for the first block).
/// 3. From the **second** Ecotone block onward: `L1BlockInfoTx::Ecotone`
///    (with `blob_base_fee`, `blob_base_fee_scalar`, new selector).
///
/// `operator_fees.rs` tests the Isthmus→Jovian transition; this covers the
/// earlier pre-Ecotone → Ecotone boundary.
#[test]
fn ecotone_l1_info_format_transitions_at_activation() {
    let batcher_cfg = BatcherConfig::default();

    // Canyon and Delta active at genesis; Ecotone activates at ts=6 (block 3,
    // block_time=2). All earlier forks silent so they don't interfere.
    let ecotone_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(ecotone_time),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Block 1: ts=2 — pre-Ecotone. Expect Bedrock format.
    let block1 = builder.build_next_block().expect("build block 1");
    let info1 = l1_info_from_block(&block1);
    assert!(
        matches!(info1, L1BlockInfoTx::Bedrock(_)),
        "block 1 (pre-Ecotone ts=2) must use Bedrock format, got {info1:?}"
    );

    // Block 2: ts=4 — still pre-Ecotone. Expect Bedrock format.
    let block2 = builder.build_next_block().expect("build block 2");
    let info2 = l1_info_from_block(&block2);
    assert!(
        matches!(info2, L1BlockInfoTx::Bedrock(_)),
        "block 2 (pre-Ecotone ts=4) must use Bedrock format, got {info2:?}"
    );

    // Block 3: ts=6 — FIRST Ecotone block. Protocol rule: L1Block contract
    // upgrade tx is appended AFTER the L1 info deposit, so the contract is
    // still on the Bedrock ABI. The sequencer sends a Bedrock-format L1 info tx.
    let block3 = builder.build_empty_block().expect("build empty block 3 (first Ecotone)");
    assert_eq!(block3.header.timestamp, ecotone_time, "block 3 must be at ecotone_time");
    let info3 = l1_info_from_block(&block3);
    assert!(
        matches!(info3, L1BlockInfoTx::Bedrock(_)),
        "block 3 (first Ecotone ts=6) must still use Bedrock format, got {info3:?}"
    );

    // Block 4: ts=8 — second Ecotone block. L1Block contract now upgraded.
    // Sequencer sends Ecotone-format L1 info tx.
    let block4 = builder.build_next_block().expect("build block 4 (post-Ecotone)");
    let info4 = l1_info_from_block(&block4);
    assert!(
        matches!(info4, L1BlockInfoTx::Ecotone(_)),
        "block 4 (post-Ecotone ts=8) must use Ecotone format, got {info4:?}"
    );
}

// ---------------------------------------------------------------------------
// B. Ecotone activation block user txs are accepted at the batch layer
//
// op-e2e ref: ecotone_fork_test.go
// ---------------------------------------------------------------------------

/// Unlike the Jovian hardfork, Ecotone does **not** enforce an empty first block
/// at the batch-validation layer. There is no `NonEmptyTransitionBlock` check
/// for Ecotone; the constraint is enforced at the sequencer level
/// (`should_use_tx_pool()` returns `false` for the first Ecotone block).
///
/// This test verifies that a batch that *includes* user transactions for the
/// Ecotone activation block (ts=6) is **accepted** by the pipeline and all 4
/// L2 blocks derive successfully:
///
/// - Blocks 1–2: pre-Ecotone, user txs accepted.
/// - Block 3 (ts=6): batch with user txs accepted; pipeline also injects
///   Ecotone upgrade transactions via `StatefulAttributesBuilder`.
/// - Block 4 (ts=8): derives normally after block 3.
///
/// `operator_fees.rs` mirrors this kind of test for the Isthmus→Jovian boundary
/// where `NonEmptyTransitionBlock` *does* fire.
#[tokio::test]
async fn ecotone_activation_block_user_txs_accepted_at_batch_layer() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    // Canyon and Delta active at genesis; Ecotone at ts=6 (block 3).
    // Fjord must be active so the batcher's brotli-compressed frames are
    // accepted by the pipeline's BatchReader.
    let ecotone_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(ecotone_time),
        fjord_time: Some(0),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Blocks 1 and 2: pre-Ecotone, user txs OK.
    for _ in 1..=2u64 {
        let block = builder.build_next_block().expect("build pre-Ecotone block");
        let mut source = ActionL2Source::new();
        source.push(block);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone())
            .advance(&mut h.l1)
            .await
            .expect("advance pre-Ecotone batch");
    }

    // Block 3 at ts=6 (first Ecotone): build WITH a user tx. Unlike Jovian,
    // Ecotone has no NonEmptyTransitionBlock batch check, so this batch is
    // accepted and block 3 is NOT deposit-only.
    let block3_with_user_tx = builder.build_next_block().expect("build block 3 with user tx");
    assert_eq!(
        block3_with_user_tx.header.timestamp, ecotone_time,
        "block 3 must land exactly at ecotone_time"
    );

    let mut source3 = ActionL2Source::new();
    source3.push(block3_with_user_tx);
    Batcher::new(source3, &h.rollup_config, batcher_cfg.clone())
        .advance(&mut h.l1)
        .await
        .expect("advance block 3 batch");

    // Block 4: post-Ecotone, user txs OK.
    let block4 = builder.build_next_block().expect("build post-Ecotone block 4");
    let mut source4 = ActionL2Source::new();
    source4.push(block4);
    Batcher::new(source4, &h.rollup_config, batcher_cfg)
        .advance(&mut h.l1)
        .await
        .expect("advance block 4 batch");

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await.expect("initialize");

    // Drive derivation through all 4 L1 blocks.
    for i in 1..=4u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("pipeline");
    }

    // Ecotone has no NonEmptyTransitionBlock batch check, so block 3's batch
    // (with user txs) is accepted. All 4 blocks must derive: block 3 is NOT
    // deposit-only, unlike the Jovian transition block.
    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "safe head must reach block 4; Ecotone has no batch-level empty-block enforcement"
    );
}

// ---------------------------------------------------------------------------
// C. Derivation through the Ecotone activation boundary
//
// op-e2e ref: ecotone_fork_test.go
// ---------------------------------------------------------------------------

/// Full end-to-end derivation through the Ecotone activation boundary,
/// following the same pattern as `jovian_derivation_crosses_activation_boundary`
/// in `hardfork_activation.rs`.
///
/// - Canyon and Delta active at genesis (via Fjord cascade ensures brotli is
///   accepted by the verifier's `BatchReader`).
/// - Ecotone activates at ts=6 (L2 block 3, `block_time=2`).
/// - Blocks 1–2: pre-Ecotone, submitted with user transactions.
/// - Block 3: first Ecotone block — submitted **empty** (no user txs) because
///   the pipeline prepends Ecotone upgrade transactions to this block.
/// - Block 4: post-Ecotone, user txs OK.
///
/// All 4 L2 blocks must be derived.
#[tokio::test]
async fn ecotone_derivation_crosses_activation_boundary() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    // All forks through Delta active at genesis so that at ts=6 only Ecotone
    // is "new". Fjord must be active so the batcher's brotli compression is
    // accepted. Ecotone activates at ts=6 (block 3).
    let ecotone_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(ecotone_time),
        fjord_time: Some(0),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build and submit 4 L2 blocks individually (one batch per L1 block).
    for i in 1..=4u64 {
        let block = if i == 3 {
            // First Ecotone block: must be deposit-only (no user txs) because
            // the pipeline prepends the Ecotone upgrade transactions here.
            builder.build_empty_block().expect("build empty first-Ecotone block")
        } else {
            builder.build_next_block().expect("build L2 block with user tx")
        };

        let mut source = ActionL2Source::new();
        source.push(block);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone())
            .advance(&mut h.l1)
            .await
            .expect("advance batch");
    }

    let (mut verifier, _chain) = h.create_verifier_from_sequencer(
        &builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    verifier.initialize().await.expect("initialize");

    for i in 1..=4u64 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("pipeline");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "derivation must succeed through the Ecotone activation boundary"
    );
}
