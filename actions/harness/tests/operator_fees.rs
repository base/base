#![doc = "Action tests for operator fee encoding and hardfork activation."]

use alloy_primitives::{Address, B256, LogData};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, block_info_from,
};
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::{
    CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC, HardForkConfig, RollupConfig,
};
use base_consensus_registry::Registry;
use base_protocol::L1BlockInfoTx;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a [`RollupConfig`] for operator fee tests with the given hardfork schedule.
///
/// Starts from the real Base mainnet config and zeroes the genesis fields for
/// in-memory testing (L1 miner starts at ts=0). The caller supplies the
/// complete [`HardForkConfig`].
fn rollup_config_for(batcher: &BatcherConfig, hardforks: HardForkConfig) -> RollupConfig {
    let mut rc = Registry::rollup_config(8453).expect("mainnet config").clone();
    rc.batch_inbox_address = batcher.inbox_address;
    rc.genesis.system_config.as_mut().unwrap().batcher_address = batcher.batcher_address;
    rc.genesis.l2_time = 0;
    rc.genesis.l1 = Default::default();
    rc.genesis.l2 = Default::default();
    rc.hardforks = hardforks;
    rc
}

/// Decode the [`L1BlockInfoTx`] from the first (L1 info deposit) transaction of
/// an [`OpBlock`].
///
/// The first tx in every L2 block is always the L1 info deposit. Its calldata
/// is the `setL1BlockValues*` ABI-encoded call, from which [`L1BlockInfoTx`]
/// extracts the epoch information and hardfork-specific fee parameters.
fn l1_info_from_block(block: &OpBlock) -> L1BlockInfoTx {
    let OpTxEnvelope::Deposit(sealed) = &block.body.transactions[0] else {
        panic!("first transaction must be a deposit");
    };
    L1BlockInfoTx::decode_calldata(sealed.inner().input.as_ref())
        .expect("L1 info calldata must decode successfully")
}

// ---------------------------------------------------------------------------
// Section 1: L1 info format and operator fee encoding
//
// These tests inspect the L1 info deposit transaction embedded in each built
// [`OpBlock`] to verify that:
// - The correct calldata format (Ecotone / Isthmus / Jovian) is selected for
//   the active hardfork.
// - `operator_fee_scalar` and `operator_fee_constant` are zero when Isthmus is
//   inactive and match the [`SystemConfig`] once Isthmus is active.
//
// No L2 derivation is performed — the tests drive only the [`L2Sequencer`].
// ---------------------------------------------------------------------------

/// Pre-Isthmus L2 blocks carry an `Ecotone`-format L1 info deposit.
///
/// Because Isthmus has not activated, the `operator_fee_scalar` and
/// `operator_fee_constant` fields are absent from the calldata; the accessors
/// on [`L1BlockInfoTx`] return zero for pre-Isthmus variants.
///
/// All forks through Holocene are activated at genesis so that only Isthmus
/// (and later) are absent. This ensures the Ecotone-format assertion reflects
/// the intended pre-Isthmus state and not an accidentally inactive earlier fork.
#[test]
fn operator_fee_not_encoded_before_isthmus() {
    let batcher_cfg = BatcherConfig::default();
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        // Isthmus and Jovian are intentionally absent — they remain None so
        // their mainnet timestamps are unreachable (L1 miner starts at ts=0).
        ..Default::default()
    };
    let rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build one block and verify it uses the Ecotone format with zero operator fees.
    let block = builder.build_next_block().expect("build block");
    let l1_info = l1_info_from_block(&block);

    assert!(
        matches!(l1_info, L1BlockInfoTx::Ecotone(_)),
        "pre-Isthmus L1 info must use Ecotone format, got {l1_info:?}"
    );
    assert_eq!(l1_info.operator_fee_scalar(), 0, "operator_fee_scalar must be zero before Isthmus");
    assert_eq!(
        l1_info.operator_fee_constant(),
        0,
        "operator_fee_constant must be zero before Isthmus"
    );
}

/// Post-Isthmus L2 blocks carry an `Isthmus`-format L1 info deposit that
/// encodes `operator_fee_scalar` and `operator_fee_constant` from the genesis
/// [`SystemConfig`].
///
/// Setting `isthmus_time = Some(0)` makes Isthmus active at genesis. Because
/// `is_first_isthmus_block(ts)` checks
/// `is_isthmus_active(ts) && !is_isthmus_active(ts − block_time)`, and with
/// `isthmus_time = 0` the result is `true && !true = false` for every positive
/// timestamp, no built block is treated as the transition block. Every
/// sequencer-built block (ts ≥ 2) uses the full Isthmus calldata format.
#[test]
fn operator_fee_encoded_in_l1_info_post_isthmus() {
    const OPERATOR_FEE_SCALAR: u32 = 2_000;
    const OPERATOR_FEE_CONSTANT: u64 = 500;

    let batcher_cfg = BatcherConfig::default();
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(0),
        ..Default::default()
    };
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build one block and verify it uses the Isthmus format with the configured fee params.
    let block = builder.build_next_block().expect("build block");
    let l1_info = l1_info_from_block(&block);

    assert!(
        matches!(l1_info, L1BlockInfoTx::Isthmus(_)),
        "post-Isthmus L1 info must use Isthmus format, got {l1_info:?}"
    );
    assert_eq!(
        l1_info.operator_fee_scalar(),
        OPERATOR_FEE_SCALAR,
        "operator_fee_scalar must match the system config"
    );
    assert_eq!(
        l1_info.operator_fee_constant(),
        OPERATOR_FEE_CONSTANT,
        "operator_fee_constant must match the system config"
    );
}

/// The L1 info format transitions from `Ecotone` to `Isthmus` across three
/// stages: pre-activation (Ecotone, zero fees), the first Isthmus block
/// (still Ecotone, zero fees), then all subsequent blocks (Isthmus, non-zero
/// fees).
///
/// The first Isthmus block uses the old format because `L1BlockInfoTx::try_new`
/// has an `!is_first_isthmus_block` guard: when the guard fires the code falls
/// through to the `Ecotone` branch. This allows the Isthmus upgrade deposit
/// transactions (injected by the derivation pipeline) to update the `L1Block`
/// contract before the new calldata format is consumed — exactly the same
/// mechanism as the Ecotone/Bedrock transition. From the second Isthmus block
/// onwards the `Isthmus` format is used and operator fee fields are populated.
///
/// This test verifies all three stages explicitly. Note that Granite and
/// Holocene are active from genesis so that no spurious cascade activation
/// occurs at the Isthmus boundary.
#[test]
fn l1_info_format_transitions_at_isthmus_boundary() {
    const OPERATOR_FEE_SCALAR: u32 = 1_500;
    const OPERATOR_FEE_CONSTANT: u64 = 300;

    let batcher_cfg = BatcherConfig::default();
    // Isthmus at ts=6 → with block_time=2, block 3 (ts=6) is the first Isthmus block.
    let isthmus_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(isthmus_time),
        ..Default::default()
    };
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Stage 1: Blocks 1-2 (ts=2, 4) — pre-Isthmus → Ecotone format, zero operator fees.
    for i in 1u64..=2 {
        let block = builder.build_next_block().expect("build pre-Isthmus block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Ecotone(_)),
            "block {i}: pre-Isthmus must use Ecotone format"
        );
        assert_eq!(l1_info.operator_fee_scalar(), 0, "block {i}: no operator fee pre-Isthmus");
        assert_eq!(l1_info.operator_fee_constant(), 0, "block {i}: no operator fee pre-Isthmus");
    }

    // Stage 2: Block 3 (ts=6) — first Isthmus block, still Ecotone format.
    // `is_first_isthmus_block(6)` returns true, so `try_new` skips the Isthmus
    // branch and falls through to Ecotone. Operator fee fields remain zero here;
    // the upgrade deposit transactions land in the same block to upgrade the
    // L1Block contract, enabling the Isthmus format from block 4 onwards.
    {
        let block = builder.build_next_block().expect("build first Isthmus block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Ecotone(_)),
            "block 3 (first Isthmus): L1 info must use Ecotone format at the transition"
        );
        assert_eq!(
            l1_info.operator_fee_scalar(),
            0,
            "block 3: operator fee not yet active at the Isthmus transition block"
        );
        assert_eq!(
            l1_info.operator_fee_constant(),
            0,
            "block 3: operator fee not yet active at the Isthmus transition block"
        );
    }

    // Stage 3: Block 4 (ts=8) — second Isthmus block, Isthmus format, fees active.
    {
        let block = builder.build_next_block().expect("build second Isthmus block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Isthmus(_)),
            "block 4 (second Isthmus): L1 info must use Isthmus format"
        );
        assert_eq!(
            l1_info.operator_fee_scalar(),
            OPERATOR_FEE_SCALAR,
            "block 4: operator_fee_scalar must be active from the second Isthmus block"
        );
        assert_eq!(
            l1_info.operator_fee_constant(),
            OPERATOR_FEE_CONSTANT,
            "block 4: operator_fee_constant must be active from the second Isthmus block"
        );
    }
}

/// The L1 info format transitions from `Isthmus` to `Jovian` across three
/// stages: pre-activation (Isthmus format), the first Jovian block (still
/// Isthmus format), then all subsequent blocks (Jovian format).
///
/// The first Jovian block uses the Isthmus format for the same reason as the
/// Isthmus transition: `L1BlockInfoTx::try_new` has a `!is_first_jovian_block`
/// guard that, when triggered, skips the Jovian branch and falls through to the
/// Isthmus branch (which fires because `is_isthmus_active` is true). The upgrade
/// deposits update the `L1Block` contract in that same block; from the second
/// Jovian block onwards the `Jovian` format is used.
///
/// Additionally, the first Jovian block must contain no user transactions —
/// the batch validator enforces `NonEmptyTransitionBlock` and drops any batch
/// with user txs at the Jovian transition. `build_empty_block()` is used here
/// to respect that constraint.
///
/// The `operator_fee_scalar` and `operator_fee_constant` values are identical
/// across the Isthmus and Jovian formats. The formula difference — Jovian
/// multiplies `gas * scalar * 100` while Isthmus divides `gas * scalar / 1_000_000`
/// — is an EVM execution detail, not a change in how values are encoded in the
/// L1 info deposit.
#[test]
fn l1_info_format_transitions_at_jovian_boundary() {
    const OPERATOR_FEE_SCALAR: u32 = 1_000;
    const OPERATOR_FEE_CONSTANT: u64 = 10;

    let batcher_cfg = BatcherConfig::default();
    // Isthmus at genesis, Jovian at ts=6 → block 3 (ts=6) is the first Jovian block.
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
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Stage 1: Blocks 1-2 (ts=2, 4) — Isthmus active, Jovian not yet → Isthmus format.
    for i in 1u64..=2 {
        let block = builder.build_next_block().expect("build pre-Jovian block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Isthmus(_)),
            "block {i}: pre-Jovian must use Isthmus format"
        );
        assert_eq!(l1_info.operator_fee_scalar(), OPERATOR_FEE_SCALAR);
        assert_eq!(l1_info.operator_fee_constant(), OPERATOR_FEE_CONSTANT);
    }

    // Stage 2: Block 3 (ts=6) — first Jovian block, still Isthmus format.
    // `is_first_jovian_block(6)` returns true, so `try_new` skips the Jovian
    // branch and falls through to the Isthmus branch. The block must be empty
    // because the batch validator enforces `NonEmptyTransitionBlock` for Jovian.
    {
        let block = builder.build_empty_block().expect("build empty first Jovian block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Isthmus(_)),
            "block 3 (first Jovian): L1 info must use Isthmus format at the transition"
        );
        assert_eq!(l1_info.operator_fee_scalar(), OPERATOR_FEE_SCALAR);
        assert_eq!(l1_info.operator_fee_constant(), OPERATOR_FEE_CONSTANT);
    }

    // Stage 3: Block 4 (ts=8) — second Jovian block, Jovian format.
    // The operator fee scalar and constant are unchanged; only the EVM formula
    // differs (gas * scalar * 100 vs gas * scalar / 1_000_000 for Isthmus).
    {
        let block = builder.build_next_block().expect("build second Jovian block");
        let l1_info = l1_info_from_block(&block);

        assert!(
            matches!(l1_info, L1BlockInfoTx::Jovian(_)),
            "block 4 (second Jovian): L1 info must use Jovian format"
        );
        assert_eq!(
            l1_info.operator_fee_scalar(),
            OPERATOR_FEE_SCALAR,
            "block 4: operator_fee_scalar must be preserved in Jovian format"
        );
        assert_eq!(
            l1_info.operator_fee_constant(),
            OPERATOR_FEE_CONSTANT,
            "block 4: operator_fee_constant must be preserved in Jovian format"
        );
    }
}

// ---------------------------------------------------------------------------
// Section 2: Derivation tests — pipeline behaviour at operator fee boundaries
//
// These tests run the full derivation pipeline through the Isthmus operator
// fee activation boundary and verify that the pipeline accepts blocks without
// errors.
// ---------------------------------------------------------------------------

/// The derivation pipeline accepts blocks with user transactions across the
/// Isthmus operator fee activation boundary.
///
/// Unlike Jovian (which enforces `NonEmptyTransitionBlock` and requires an
/// empty transition block), Isthmus has no such restriction. All four blocks —
/// including the first Isthmus block — may carry user transactions and are
/// derived without errors.
///
/// Non-zero `operator_fee_scalar` and `operator_fee_constant` are set in the
/// genesis [`SystemConfig`] so that the fee parameters carried through the
/// pipeline are non-trivial. The verifier does not execute transactions, so
/// the actual fee amounts are not checked here; the derivation count and safe
/// head position confirm that the pipeline accepted all four batches.
///
/// Configuration: Canyon through Holocene at genesis, Isthmus at ts=6 (block 3).
///
/// ```text
///   block 1 → ts=2  pre-Isthmus, user txs ✓
///   block 2 → ts=4  pre-Isthmus, user txs ✓
///   block 3 → ts=6  first Isthmus, user txs ✓ (no empty-block requirement)
///   block 4 → ts=8  post-Isthmus, user txs ✓
/// ```
#[tokio::test]
async fn isthmus_derivation_crosses_operator_fee_boundary() {
    const OPERATOR_FEE_SCALAR: u32 = 1_200;
    const OPERATOR_FEE_CONSTANT: u64 = 400;

    let batcher_cfg = BatcherConfig::default();
    let isthmus_time = 6u64;
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(isthmus_time),
        ..Default::default()
    };
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut block_hashes = Vec::new();
    for _ in 0..4u64 {
        // All blocks carry user transactions — Isthmus allows user txs at transition.
        let mut source = ActionL2Source::new();
        source.push(builder.build_next_block().expect("build L2 block"));
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));

        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    for i in 1..=4u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("pipeline");
        assert_eq!(derived, 1, "L1 block {i} must derive exactly one L2 block");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        4,
        "safe head must reach block 4 after crossing the Isthmus operator fee boundary"
    );
}

/// The batch validator drops a non-empty batch submitted for the first Jovian
/// block, and the derivation pipeline generates a deposit-only default block
/// for that slot instead.
///
/// The first Jovian block (ts=6, block 3 with `block_time=2`) must contain no
/// user transactions. The batch validator enforces `NonEmptyTransitionBlock`:
/// any batch with user transactions at the Jovian upgrade slot is dropped.
/// When the sequencing window expires with no valid batch remaining for the
/// slot, the pipeline force-includes a deposit-only block.
///
/// Setup:
/// ```text
///   seq_window_size = 4  (epoch 0 window closes at L1 block 4)
///   Isthmus at ts=0, Jovian at ts=6 (block 3 = first Jovian block)
///
///   L1 block 1: batch for L2 block 1 (user tx) → derived normally
///   L1 block 2: batch for L2 block 2 (user tx) → derived normally
///   L1 block 3: batch for L2 block 3 (user tx, first Jovian) → DROPPED
///   L1 block 4: empty (closes epoch-0 seq window) → block 3 force-included
///               as deposit-only
/// ```
///
/// The [`derived_user_tx_counts`] accessor on the verifier confirms that the
/// pipeline generated a deposit-only block for slot 3 (count = 0) while
/// preserving the user transactions in the earlier blocks.
///
/// [`derived_user_tx_counts`]: base_action_harness::L2Verifier::derived_user_tx_counts
#[tokio::test]
async fn jovian_non_empty_transition_batch_generates_deposit_only_block() {
    let batcher_cfg = BatcherConfig::default();
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
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    // Narrow the sequencing window: epoch-0 batches must land in L1 blocks 0–3.
    // When the pipeline reaches L1 block 4 (epoch 0 + 4 = 4), force-inclusion
    // fires and a deposit-only block is generated for any pending L2 slot.
    rollup_cfg.seq_window_size = 4;

    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build three L2 blocks — each with 1 user transaction (the sequencer default).
    // Block 3 (ts=6) is the first Jovian block. The batch validator will drop the
    // non-empty batch for that slot with NonEmptyTransitionBlock.
    let mut block_hashes = Vec::new();
    for i in 1u64..=3 {
        let mut source = ActionL2Source::new();
        source.push(builder.build_next_block().expect("build L2 block"));
        let head = builder.head();
        if i < 3 {
            // Register hashes only for blocks 1–2. Block 3 will be replaced by
            // a pipeline-generated deposit-only block with a different hash, so
            // the sequencer's hash for that slot is intentionally not registered.
            block_hashes.push((head.block_info.number, head.block_info.hash));
        }

        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // Mine L1 block 4 (no batch). This closes the epoch-0 sequencing window
    // (0 + seq_window_size 4 = 4), triggering force-inclusion for L2 slot 3.
    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    // Signal L1 blocks 1–3. Blocks 1–2 are derived from their valid batches.
    // Block 3's batch is dropped (NonEmptyTransitionBlock) and the pipeline
    // stalls waiting for more L1 data (the seq window has not yet closed).
    for i in 1u64..=3 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("pipeline");
    }
    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "only blocks 1–2 derived from valid batches; block 3 pending (batch dropped)"
    );

    // Signal L1 block 4. The epoch-0 window is now closed, so the pipeline
    // force-includes L2 slot 3 as a deposit-only block.
    let l1_block_4 = block_info_from(h.l1.block_by_number(4).expect("block 4 exists"));
    verifier.act_l1_head_signal(l1_block_4).await.expect("signal block 4");
    verifier.act_l2_pipeline_full().await.expect("pipeline block 4");

    assert!(
        verifier.l2_safe().block_info.number >= 3,
        "safe head must advance past block 3 after force-inclusion"
    );

    // Verify per-block user tx counts via the verifier's tracking field.
    let counts = verifier.derived_user_tx_counts();
    let find = |n: u64| counts.iter().find(|(bn, _)| *bn == n).map(|(_, c)| *c);

    assert_eq!(find(1), Some(1), "block 1: batch accepted, 1 user tx");
    assert_eq!(find(2), Some(1), "block 2: batch accepted, 1 user tx");
    assert_eq!(
        find(3),
        Some(0),
        "block 3: batch dropped (NonEmptyTransitionBlock) → deposit-only, 0 user txs"
    );
}

// ---------------------------------------------------------------------------
// Section 3: SystemConfig update propagation
//
// These tests verify that operator fee changes committed to L1 via
// `ConfigUpdate` logs are reflected in the L1 info deposit transactions of
// subsequently derived L2 blocks. The derivation pipeline's `IndexedTraversal`
// stage reads `ConfigUpdate` logs from L1 receipts and updates its internal
// `SystemConfig`; the `StatefulAttributesBuilder` uses the updated config to
// generate the L1 info deposit for each new L2 block.
// ---------------------------------------------------------------------------

/// Build a `ConfigUpdate` log encoding an `OperatorFee` (type 5) change.
///
/// Log layout mirrors `SystemConfig.sol`:
/// - `topics[0]` = `CONFIG_UPDATE_TOPIC`
/// - `topics[1]` = `CONFIG_UPDATE_EVENT_VERSION_0`
/// - `topics[2]` = `B256` encoding `UpdateType::OperatorFee = 5`
/// - `data`      = 96 bytes: pointer(32=0x20) + length(32=0x20) +
///   payload(32 bytes): `[0..20]` zeros | `[20..24]` scalar (u32 BE) |
///   `[24..32]` constant (u64 BE)
fn operator_fee_update_log(
    l1_sys_cfg_addr: Address,
    operator_fee_scalar: u32,
    operator_fee_constant: u64,
) -> alloy_primitives::Log {
    let mut data = [0u8; 96];
    data[31] = 0x20; // pointer = 32
    data[63] = 0x20; // length  = 32
    // Payload offsets within the 96-byte array:
    //   payload[20..24] = data[84..88] = scalar (u32 BE)
    //   payload[24..32] = data[88..96] = constant (u64 BE)
    data[84..88].copy_from_slice(&operator_fee_scalar.to_be_bytes());
    data[88..96].copy_from_slice(&operator_fee_constant.to_be_bytes());

    let mut update_type = [0u8; 32];
    update_type[31] = 5; // OperatorFee = 5

    alloy_primitives::Log {
        address: l1_sys_cfg_addr,
        data: LogData::new_unchecked(
            vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, B256::from(update_type)],
            data.into(),
        ),
    }
}

/// An `OperatorFee` system-config update committed to L1 is reflected in the
/// L1 info deposit transactions of derived L2 blocks once the L1 epoch advances
/// to the block containing the update.
///
/// `StatefulAttributesBuilder` re-reads the system config from the L2 provider
/// on every block, then additionally calls `update_with_receipts()` on the
/// freshly-fetched config only when the L2 epoch changes (i.e., the L2 block's
/// L1 origin differs from its parent's L1 origin). This means a `ConfigUpdate`
/// log in L1 block N is invisible to the attributes builder until the first L2
/// block whose epoch advances to N.
///
/// With L1 `block_time=12` s and L2 `block_time=2` s, six L2 blocks fit in one L1
/// epoch (genesis counts as block 0). The sequencer advances the epoch when
/// `next_l1.timestamp <= next_l2.timestamp`. L1 block 1 has ts=12, so the
/// epoch transitions at L2 block 6 (ts=12):
///
/// ```text
///   Pre-mined:
///     L1 block 1 (ts=12): OperatorFee update log
///   L2 blocks (L2 block_time = 2 s):
///     Block 0 – genesis    (ts= 0) epoch 0  – genesis state, not derived
///     Block 1  (ts= 2)     epoch 0  – OLD fee params
///     Block 2  (ts= 4)     epoch 0  – OLD fee params
///     Block 3  (ts= 6)     epoch 0  – OLD fee params
///     Block 4  (ts= 8)     epoch 0  – OLD fee params
///     Block 5  (ts=10)     epoch 0  – OLD fee params
///     Block 6  (ts=12)     epoch 1  ← epoch change: reads L1 block 1 receipts
///                                     → NEW fee params applied
///   Batched in L1:
///     L1 block 2: batches for L2 blocks 1–5
///     L1 block 3: batch  for L2 block 6
/// ```
///
/// Configuration: all forks through Isthmus are active at genesis so the
/// Isthmus-format L1 info deposit (which includes operator fee fields) is
/// used from the first block. Jovian is intentionally absent.
#[tokio::test]
async fn operator_fee_config_update_propagates_to_l1_info() {
    const OLD_SCALAR: u32 = 1_000;
    const OLD_CONSTANT: u64 = 500;
    const NEW_SCALAR: u32 = 3_000;
    const NEW_CONSTANT: u64 = 700;

    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_cfg = BatcherConfig::default();
    let hardforks = HardForkConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(0),
        fjord_time: Some(0),
        granite_time: Some(0),
        holocene_time: Some(0),
        isthmus_time: Some(0),
        ..Default::default()
    };
    let mut rollup_cfg = rollup_config_for(&batcher_cfg, hardforks);
    rollup_cfg.l1_system_config_address = l1_sys_cfg_addr;
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OLD_SCALAR);
    sys_cfg.operator_fee_constant = Some(OLD_CONSTANT);

    // Standard L1 block_time (12 s). With L2 block_time=2 s, six L2 blocks
    // fit in each L1 epoch. The epoch change from 0 to 1 occurs at L2 block 6
    // (ts=12), when ts(L1 block 1)=12 ≤ ts(L2 block 6)=12.
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Pre-mine L1 block 1 (ts=12) with the OperatorFee update so the sequencer
    // can reference epoch 1 when it builds L2 block 6.
    h.l1.enqueue_log(operator_fee_update_log(l1_sys_cfg_addr, NEW_SCALAR, NEW_CONSTANT));
    h.l1.mine_block(); // L1 block 1, ts=12

    // Snapshot the chain (blocks 0 and 1) before building L2 blocks. The sequencer
    // needs L1 block 1 in its chain to advance the epoch at L2 block 6.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // L2 blocks 1–5 (ts=2,4,6,8,10): epoch 0, OLD config.
    let mut epoch0_blocks: Vec<base_alloy_consensus::OpBlock> = Vec::new();
    let mut epoch0_hashes: Vec<B256> = Vec::new();
    for _ in 0..5 {
        let block = sequencer.build_next_block().expect("build epoch-0 block");
        epoch0_hashes.push(sequencer.head().block_info.hash);
        epoch0_blocks.push(block);
    }

    // L2 block 6 (ts=12): epoch 1, epoch change — NEW config from L1 block 1's receipts.
    let block6 = sequencer.build_next_block().expect("build block 6");
    let hash6 = sequencer.head().block_info.hash;

    // Batch all epoch-0 blocks into L1 block 2 and block 6 into L1 block 3.
    let mut source = ActionL2Source::new();
    for block in epoch0_blocks {
        source.push(block);
    }
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    batcher.advance().expect("encode blocks 1–5");
    drop(batcher);
    h.l1.mine_block(); // L1 block 2, ts=24

    let mut source = ActionL2Source::new();
    source.push(block6);
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode block 6");
    drop(batcher);
    h.l1.mine_block(); // L1 block 3, ts=36

    // Verifier snapshot includes all L1 blocks 0–3.
    let (mut verifier, _chain) = h.create_verifier();
    for (i, hash) in epoch0_hashes.iter().enumerate() {
        verifier.register_block_hash((i + 1) as u64, *hash);
    }
    verifier.register_block_hash(6, hash6);
    verifier.initialize().await.expect("initialize");

    for i in 1u64..=3 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(verifier.l2_safe().block_info.number, 6, "all 6 L2 blocks must be derived");

    let infos = verifier.derived_l1_info_txs();
    let find = |n: u64| infos.iter().find(|(bn, _)| *bn == n).map(|(_, tx)| tx);

    // Blocks 1–5 (epoch 0, no receipt update) carry OLD fee params.
    for n in 1u64..=5 {
        let info = find(n).unwrap_or_else(|| panic!("L1 info tx for block {n} must be recorded"));
        assert_eq!(
            info.operator_fee_scalar(),
            OLD_SCALAR,
            "block {n}: operator_fee_scalar must reflect the genesis SystemConfig"
        );
        assert_eq!(
            info.operator_fee_constant(),
            OLD_CONSTANT,
            "block {n}: operator_fee_constant must reflect the genesis SystemConfig"
        );
    }

    // Block 6 (epoch 1 — first epoch change) carries NEW fee params. This is the
    // "seventh" block total counting from genesis (block 0), confirming that
    // StatefulAttributesBuilder reads L1 block 1's receipts on the epoch change.
    let info6 = find(6).expect("L1 info tx for block 6 must be recorded");
    assert_eq!(
        info6.operator_fee_scalar(),
        NEW_SCALAR,
        "block 6: operator_fee_scalar must reflect the OperatorFee config update"
    );
    assert_eq!(
        info6.operator_fee_constant(),
        NEW_CONSTANT,
        "block 6: operator_fee_constant must reflect the OperatorFee config update"
    );
}
