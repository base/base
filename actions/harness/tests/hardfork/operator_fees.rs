//! Action tests for operator fee encoding and hardfork activation.

use alloy_primitives::Address;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, ForkMatrix, L1MinerConfig,
    SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_protocol::L1BlockInfoTx;

// ---------------------------------------------------------------------------
// Section 1: L1 info format and operator fee encoding
//
// These tests inspect the L1 info deposit transaction embedded in each built
// [`BaseBlock`] to verify that:
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
/// This assertion holds for every post-Granite pre-Isthmus fork. Running the
/// same test across Granite, Holocene, and the pectra-blob-schedule patch keeps
/// the invariant covered as later forks are added to the harness matrix.
#[tokio::test]
async fn operator_fee_not_encoded_before_isthmus() {
    let batcher_cfg = BatcherConfig::default();
    ForkMatrix::pre_isthmus()
        .run_async(|fork_name, hardforks| {
            let batcher_cfg = batcher_cfg.clone();
            async move {
                let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
                    .with_hardforks(hardforks)
                    .build();
                let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
                let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
                let mut builder = h.create_l2_sequencer(l1_chain);

                // Build one block and verify it uses the Ecotone format with zero operator fees.
                let block = builder.build_next_block_with_single_transaction().await;
                let l1_info = ActionTestHarness::l1_info_from_block(&block);

                assert!(
                    matches!(l1_info, L1BlockInfoTx::Ecotone(_)),
                    "{fork_name}: pre-Isthmus L1 info must use Ecotone format, got {l1_info:?}"
                );
                assert_eq!(
                    l1_info.operator_fee_scalar(),
                    0,
                    "{fork_name}: operator_fee_scalar must be zero before Isthmus"
                );
                assert_eq!(
                    l1_info.operator_fee_constant(),
                    0,
                    "{fork_name}: operator_fee_constant must be zero before Isthmus"
                );
            }
        })
        .await;
}

/// From Isthmus onward, L2 blocks carry the active hardfork's L1 info format
/// with `operator_fee_scalar` and `operator_fee_constant` encoded from the
/// genesis [`SystemConfig`].
///
/// Setting fork activation times to `Some(0)` makes all forks active at genesis.
/// Because `is_first_<fork>_block(ts)` checks `is_active(ts) && !is_active(ts −
/// block_time)`, with activation at 0 the condition is `true && !true = false`
/// for every positive timestamp — no sequencer-built block is treated as the
/// transition block. Every block (ts ≥ 2) uses the full active hardfork format.
///
/// The matrix keeps the same invariant covered across both reachable post-Isthmus
/// forks: Isthmus itself and Jovian.
#[tokio::test]
async fn operator_fee_encoded_in_l1_info_from_isthmus_onward() {
    const OPERATOR_FEE_SCALAR: u32 = 2_000;
    const OPERATOR_FEE_CONSTANT: u64 = 500;

    let batcher_cfg = BatcherConfig::default();
    ForkMatrix::from_isthmus()
        .run_async(|fork_name, hardforks| {
            let batcher_cfg = batcher_cfg.clone();
            async move {
                let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
                    .with_hardforks(hardforks)
                    .build();
                let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
                sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
                sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

                let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
                let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
                let mut builder = h.create_l2_sequencer(l1_chain);

                // Build one block and verify it uses the active post-Isthmus format with
                // the configured operator fee params.
                let block = builder.build_next_block_with_single_transaction().await;
                let l1_info = ActionTestHarness::l1_info_from_block(&block);

                let expected_format = matches!(
                    (fork_name, &l1_info),
                    ("isthmus", L1BlockInfoTx::Isthmus(_)) | ("jovian", L1BlockInfoTx::Jovian(_))
                );
                assert!(
                    expected_format,
                    "{fork_name}: post-Isthmus L1 info must use the active hardfork format, got {l1_info:?}"
                );
                assert_eq!(
                    l1_info.operator_fee_scalar(),
                    OPERATOR_FEE_SCALAR,
                    "{fork_name}: operator_fee_scalar must match the system config"
                );
                assert_eq!(
                    l1_info.operator_fee_constant(),
                    OPERATOR_FEE_CONSTANT,
                    "{fork_name}: operator_fee_constant must match the system config"
                );
            }
        })
        .await;
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
#[tokio::test]
async fn l1_info_format_transitions_at_isthmus_boundary() {
    const OPERATOR_FEE_SCALAR: u32 = 1_500;
    const OPERATOR_FEE_CONSTANT: u64 = 300;

    let batcher_cfg = BatcherConfig::default();
    // Isthmus at ts=6 → with block_time=2, block 3 (ts=6) is the first Isthmus block.
    let isthmus_time = 6u64;
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_holocene()
        .with_isthmus_at(isthmus_time)
        .build();
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Stage 1: Blocks 1-2 (ts=2, 4) — pre-Isthmus → Ecotone format, zero operator fees.
    for i in 1u64..=2 {
        let block = builder.build_next_block_with_single_transaction().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
        let block = builder.build_next_block_with_single_transaction().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
        let block = builder.build_next_block_with_single_transaction().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
#[tokio::test]
async fn l1_info_format_transitions_at_jovian_boundary() {
    const OPERATOR_FEE_SCALAR: u32 = 1_000;
    const OPERATOR_FEE_CONSTANT: u64 = 10;

    let batcher_cfg = BatcherConfig::default();
    // Isthmus at genesis, Jovian at ts=6 → block 3 (ts=6) is the first Jovian block.
    let jovian_time = 6u64;
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(jovian_time)
        .build();
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Stage 1: Blocks 1-2 (ts=2, 4) — Isthmus active, Jovian not yet → Isthmus format.
    for i in 1u64..=2 {
        let block = builder.build_next_block_with_single_transaction().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
        let block = builder.build_empty_block().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
        let block = builder.build_next_block_with_single_transaction().await;
        let l1_info = ActionTestHarness::l1_info_from_block(&block);

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
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_holocene()
        .with_isthmus_at(isthmus_time)
        .build();
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OPERATOR_FEE_SCALAR);
    sys_cfg.operator_fee_constant = Some(OPERATOR_FEE_CONSTANT);

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..batcher_cfg
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut isthmus_block_hash = Default::default();
    let mut block4_hash = Default::default();
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for i in 1..=4u64 {
        // All blocks carry user transactions — Isthmus allows user txs at transition.
        let block = builder.build_next_block_with_single_transaction().await;
        if i == 3 {
            isthmus_block_hash = block.header.hash_slow();
        }
        if i == 4 {
            block4_hash = block.header.hash_slow();
        }
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    // The Isthmus activation block (block 3) has upgrade deposit transactions
    // injected by the derivation pipeline that the sequencer did not execute.
    // These contract deployments change the EVM state root, so we clear the
    // sequencer's registered state root for blocks 3 and 4 to skip state-root
    // validation for those blocks. Block 4's root also differs because it is
    // derived starting from the pipeline's block 3 state (which includes the
    // Isthmus upgrade contracts), while the sequencer computed it from its own
    // block 3 state (without those contracts).
    node.register_block_hash(3, isthmus_block_hash);
    node.register_block_hash(4, block4_hash);
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 4, "all 4 L2 blocks must be derived");
    assert_eq!(
        node.l2_safe_number(),
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
/// [`derived_user_tx_counts`]: base_action_harness::TestRollupNode::derived_user_tx_counts
#[tokio::test]
async fn jovian_non_empty_transition_batch_generates_deposit_only_block() {
    let batcher_cfg = BatcherConfig::default();
    let jovian_time = 6u64;
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(jovian_time)
        .build();
    // Narrow the sequencing window: epoch-0 batches must land in L1 blocks 0–3.
    // When the pipeline reaches L1 block 4 (epoch 0 + 4 = 4), force-inclusion
    // fires and a deposit-only block is generated for any pending L2 slot.
    rollup_cfg.seq_window_size = 4;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..batcher_cfg
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build three L2 blocks — each with 1 user transaction (the sequencer default).
    // Block 3 (ts=6) is the first Jovian block. The batch validator will drop the
    // non-empty batch for that slot with NonEmptyTransitionBlock.
    let mut jovian_block_hash = Default::default();
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for i in 1u64..=3 {
        let block = builder.build_next_block_with_single_transaction().await;
        if i == 3 {
            jovian_block_hash = block.header.hash_slow();
        }
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    // Create the node with only L1 blocks 0–3 visible. Block 4 (which closes
    // the epoch-0 seq window) is pushed to the shared chain after verifying the
    // intermediate state, so the first run_until_idle cannot see it yet.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    // The first Jovian block (block 3) is force-included as a deposit-only block by
    // the pipeline (the non-empty batch is dropped). The derived block has different
    // transactions than the sequencer's version, producing a different state root.
    // Clear the sequencer's registered state root for block 3 to skip validation.
    node.register_block_hash(3, jovian_block_hash);
    node.initialize().await;

    // Signal L1 blocks 1–3. Blocks 1–2 are derived from their valid batches.
    // Block 3's batch is dropped (NonEmptyTransitionBlock) and the pipeline
    // stalls waiting for more L1 data (the seq window has not yet closed).
    node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        2,
        "only blocks 1–2 derived from valid batches; block 3 pending (batch dropped)"
    );

    // Mine L1 block 4 (no batch) and make it visible. This closes the epoch-0
    // sequencing window (0 + seq_window_size 4 = 4), triggering force-inclusion.
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());

    // Signal L1 block 4. The epoch-0 window is now closed, so the pipeline
    // force-includes L2 slot 3 as a deposit-only block.
    node.run_until_idle().await;

    assert!(
        node.l2_safe_number() >= 3,
        "safe head must advance past block 3 after force-inclusion"
    );

    // Verify per-block user tx counts via the node's tracking field.
    let counts = node.derived_user_tx_counts();
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
// subsequently derived L2 blocks. The derivation pipeline's traversal stage
// reads `ConfigUpdate` logs from L1 receipts and updates its internal
// `SystemConfig`; the `StatefulAttributesBuilder` uses the updated config to
// generate the L1 info deposit for each new L2 block.
// ---------------------------------------------------------------------------

/// Build a `ConfigUpdate` log encoding an `OperatorFee` (type 5) change.
///
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
    let mut rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_isthmus().build();
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
    h.l1.enqueue_operator_fee_update(l1_sys_cfg_addr, NEW_SCALAR, NEW_CONSTANT);
    h.l1.mine_block(); // L1 block 1, ts=12

    // Snapshot the chain (blocks 0 and 1) before building L2 blocks. The sequencer
    // needs L1 block 1 in its chain to advance the epoch at L2 block 6.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // L2 blocks 1–5 (ts=2,4,6,8,10): epoch 0, OLD config.
    let mut epoch0_blocks: Vec<base_common_consensus::BaseBlock> = Vec::new();
    for _ in 0..5 {
        let block = sequencer.build_next_block_with_single_transaction().await;
        epoch0_blocks.push(block);
    }

    // L2 block 6 (ts=12): epoch 1, epoch change — NEW config from L1 block 1's receipts.
    let block6 = sequencer.build_next_block_with_single_transaction().await;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..batcher_cfg.encoder.clone() },
        ..batcher_cfg
    };

    // Batch all epoch-0 blocks into L1 block 2 (one Batcher with all 5 blocks).
    {
        let mut source = ActionL2Source::new();
        for block in epoch0_blocks {
            source.push(block);
        }
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await; // L1 block 2, ts=24
    }

    // Batch block 6 into L1 block 3.
    {
        let mut source = ActionL2Source::new();
        source.push(block6);
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await; // L1 block 3, ts=36
    }

    // Node snapshot includes all L1 blocks 0–3.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    for _ in 1u64..=3 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 6, "all 6 L2 blocks must be derived");

    let infos = node.derived_l1_info_txs();
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
