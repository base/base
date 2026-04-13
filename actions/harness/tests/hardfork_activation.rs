//! Action tests for hardfork activation gating and cascade semantics.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_consensus_genesis::HardForkConfig;
use base_protocol::BatchType;

// ---------------------------------------------------------------------------
// Section 1: Base mainnet config — hardfork boundary tests
//
// These tests use the real Base mainnet rollup config from the chain registry
// and fast-forward through timestamps to verify that each fork activates at
// exactly the right moment and that protocol parameters change as specified.
//
// No L2 blocks are built here. The hardfork activation functions act as a
// virtual clock: querying is_X_active(timestamp ± 1) and the dynamic
// parameter methods at the exact fork boundary is sufficient to verify
// correct activation semantics.
// ---------------------------------------------------------------------------

/// Every hardfork activates at its recorded mainnet timestamp and is inactive
/// one second before that timestamp.
///
/// Timestamps are read from the config itself — no hardcoded magic numbers —
/// so this test catches any future drift between the registry and the
/// activation logic.
#[test]
fn each_hardfork_activates_at_its_mainnet_timestamp() {
    let rc = TestRollupConfigBuilder::mainnet();

    let canyon_time = rc.hardforks.canyon_time.expect("canyon_time must be set on mainnet");
    let delta_time = rc.hardforks.delta_time.expect("delta_time must be set on mainnet");
    let ecotone_time = rc.hardforks.ecotone_time.expect("ecotone_time must be set on mainnet");
    let fjord_time = rc.hardforks.fjord_time.expect("fjord_time must be set on mainnet");
    let granite_time = rc.hardforks.granite_time.expect("granite_time must be set on mainnet");
    let holocene_time = rc.hardforks.holocene_time.expect("holocene_time must be set on mainnet");
    let isthmus_time = rc.hardforks.isthmus_time.expect("isthmus_time must be set on mainnet");
    let jovian_time = rc.hardforks.jovian_time.expect("jovian_time must be set on mainnet");
    let base_v1_time = rc.hardforks.base.v1.expect("base_v1_time must be set on mainnet");

    // Canyon
    assert!(!rc.is_canyon_active(canyon_time - 1), "Canyon must be inactive before its timestamp");
    assert!(rc.is_canyon_active(canyon_time), "Canyon must be active at its timestamp");

    // Delta: only Canyon is active at delta_time - 1 (Delta cascade doesn't reach backward).
    assert!(rc.is_canyon_active(delta_time - 1), "Canyon must remain active before Delta");
    assert!(!rc.is_delta_active(delta_time - 1), "Delta must be inactive before its timestamp");
    assert!(rc.is_delta_active(delta_time), "Delta must be active at its timestamp");

    // Ecotone
    assert!(
        !rc.is_ecotone_active(ecotone_time - 1),
        "Ecotone must be inactive before its timestamp"
    );
    assert!(rc.is_ecotone_active(ecotone_time), "Ecotone must be active at its timestamp");

    // Fjord
    assert!(!rc.is_fjord_active(fjord_time - 1), "Fjord must be inactive before its timestamp");
    assert!(rc.is_fjord_active(fjord_time), "Fjord must be active at its timestamp");

    // Granite
    assert!(
        !rc.is_granite_active(granite_time - 1),
        "Granite must be inactive before its timestamp"
    );
    assert!(rc.is_granite_active(granite_time), "Granite must be active at its timestamp");

    // Holocene
    assert!(
        !rc.is_holocene_active(holocene_time - 1),
        "Holocene must be inactive before its timestamp"
    );
    assert!(rc.is_holocene_active(holocene_time), "Holocene must be active at its timestamp");

    // Isthmus
    assert!(
        !rc.is_isthmus_active(isthmus_time - 1),
        "Isthmus must be inactive before its timestamp"
    );
    assert!(rc.is_isthmus_active(isthmus_time), "Isthmus must be active at its timestamp");

    // Jovian
    assert!(!rc.is_jovian_active(jovian_time - 1), "Jovian must be inactive before its timestamp");
    assert!(rc.is_jovian_active(jovian_time), "Jovian must be active at its timestamp");

    // Base V1
    assert!(
        !rc.is_base_v1_active(base_v1_time - 1),
        "Base V1 must be inactive before its timestamp"
    );
    assert!(rc.is_base_v1_active(base_v1_time), "Base V1 must be active at its timestamp");
}

/// Hardfork timestamps on Base mainnet are strictly increasing. This guarantees
/// that no two forks activate simultaneously — each fork's `is_X_active` check
/// trips at a different second, so there is never a spurious simultaneous cascade.
#[test]
fn mainnet_hardfork_timestamps_are_strictly_ordered() {
    let h = &TestRollupConfigBuilder::mainnet().hardforks;

    let ordered: &[(&str, u64)] = &[
        ("canyon", h.canyon_time.expect("canyon_time")),
        ("delta", h.delta_time.expect("delta_time")),
        ("ecotone", h.ecotone_time.expect("ecotone_time")),
        ("fjord", h.fjord_time.expect("fjord_time")),
        ("granite", h.granite_time.expect("granite_time")),
        ("holocene", h.holocene_time.expect("holocene_time")),
        ("isthmus", h.isthmus_time.expect("isthmus_time")),
        ("jovian", h.jovian_time.expect("jovian_time")),
        ("base-v1", h.base.v1.expect("base_v1_time")),
    ];

    for pair in ordered.windows(2) {
        let (name_a, ts_a) = pair[0];
        let (name_b, ts_b) = pair[1];
        assert!(ts_a < ts_b, "{name_a} ({ts_a}) must activate strictly before {name_b} ({ts_b})");
    }
}

/// Activating a later fork implies all earlier forks via the cascade chain.
/// At each fork's exact timestamp, all preceding forks are already active,
/// and all later forks are not yet active.
#[test]
fn cascade_implies_all_preceding_forks_and_no_later_forks() {
    let rc = TestRollupConfigBuilder::mainnet();

    let delta_time = rc.hardforks.delta_time.expect("delta_time");
    let fjord_time = rc.hardforks.fjord_time.expect("fjord_time");
    let granite_time = rc.hardforks.granite_time.expect("granite_time");
    let jovian_time = rc.hardforks.jovian_time.expect("jovian_time");

    // At delta_time: Canyon is implied; Ecotone and later are not yet active.
    assert!(rc.is_canyon_active(delta_time), "Canyon must be implied at Delta");
    assert!(rc.is_delta_active(delta_time));
    assert!(!rc.is_ecotone_active(delta_time), "Ecotone must not be implied by Delta");

    // At fjord_time: Canyon through Fjord are implied; Granite and later are not.
    assert!(rc.is_canyon_active(fjord_time));
    assert!(rc.is_delta_active(fjord_time));
    assert!(rc.is_ecotone_active(fjord_time));
    assert!(rc.is_fjord_active(fjord_time));
    assert!(!rc.is_granite_active(fjord_time), "Granite must not be implied by Fjord");

    // At granite_time: Canyon through Granite are implied; Holocene and later are not.
    assert!(rc.is_fjord_active(granite_time));
    assert!(rc.is_granite_active(granite_time));
    assert!(!rc.is_holocene_active(granite_time), "Holocene must not be implied by Granite");

    // At jovian_time: all OP hardforks are implied.
    assert!(rc.is_canyon_active(jovian_time));
    assert!(rc.is_delta_active(jovian_time));
    assert!(rc.is_ecotone_active(jovian_time));
    assert!(rc.is_fjord_active(jovian_time));
    assert!(rc.is_granite_active(jovian_time));
    assert!(rc.is_holocene_active(jovian_time));
    assert!(rc.is_isthmus_active(jovian_time));
    assert!(rc.is_jovian_active(jovian_time));

    // BaseV1 is a standalone Base-specific fork; Jovian does NOT imply it.
    assert!(!rc.is_base_v1_active(jovian_time), "BaseV1 must not be implied by Jovian");
    // And setting only BaseV1 does not imply Jovian (tested in base_v1_is_standalone).
}

/// Fjord changes `max_sequencer_drift` from the per-chain configured value
/// to the protocol-defined constant 1800. The transition is sharp: one second
/// before Fjord the configured value applies; at Fjord the constant applies.
#[test]
fn fjord_changes_max_sequencer_drift_at_mainnet_timestamp() {
    let rc = TestRollupConfigBuilder::mainnet();
    let fjord_time = rc.hardforks.fjord_time.expect("fjord_time");

    // One second before Fjord: the rollup config field is used.
    assert_eq!(
        rc.max_sequencer_drift(fjord_time - 1),
        rc.max_sequencer_drift,
        "max_sequencer_drift must use the config field before Fjord"
    );

    // At Fjord activation: protocol constant 1800 (FJORD_MAX_SEQUENCER_DRIFT) overrides.
    assert_eq!(
        rc.max_sequencer_drift(fjord_time),
        1800,
        "max_sequencer_drift must be 1800 at Fjord activation"
    );
    assert_eq!(
        rc.max_sequencer_drift(fjord_time + 1),
        1800,
        "max_sequencer_drift must remain 1800 after Fjord"
    );
}

/// Granite changes `channel_timeout` from the per-chain configured value to
/// `granite_channel_timeout`. The transition is sharp at the exact fork second.
#[test]
fn granite_changes_channel_timeout_at_mainnet_timestamp() {
    let rc = TestRollupConfigBuilder::mainnet();
    let granite_time = rc.hardforks.granite_time.expect("granite_time");

    // One second before Granite: the base channel_timeout field is used.
    assert_eq!(
        rc.channel_timeout(granite_time - 1),
        rc.channel_timeout,
        "channel_timeout must use the config field before Granite"
    );

    // At Granite activation: granite_channel_timeout overrides.
    assert_eq!(
        rc.channel_timeout(granite_time),
        rc.granite_channel_timeout,
        "channel_timeout must use granite_channel_timeout at Granite activation"
    );
    assert_eq!(
        rc.channel_timeout(granite_time + 1),
        rc.granite_channel_timeout,
        "channel_timeout must remain at granite_channel_timeout after Granite"
    );
}

/// `BaseV1` is a standalone Base-specific hardfork. It is not part of the OP
/// cascade chain: Jovian does not imply it, and it does not imply Jovian.
#[test]
fn base_v1_is_standalone_from_jovian() {
    let rc = TestRollupConfigBuilder::mainnet();
    let jovian_time = rc.hardforks.jovian_time.expect("jovian_time");
    let base_v1_time = rc.hardforks.base.v1.expect("base_v1_time");

    assert!(!rc.is_base_v1_active(jovian_time), "BaseV1 must not be implied by Jovian");
    assert!(!rc.is_base_v1_active(base_v1_time - 1), "BaseV1 must remain inactive before V1");
    assert!(rc.is_base_v1_active(base_v1_time), "BaseV1 must activate at its own timestamp");
}

// ---------------------------------------------------------------------------
// Section 2: Derivation tests — pipeline behaviour at hardfork boundaries
//
// These tests exercise the full derivation pipeline with synthetic rollup
// configs. A synthetic config is necessary because the L1 miner starts at
// timestamp 0: using real mainnet timestamps (e.g. delta_time ≈ 1.7 × 10⁹)
// would require an unreachable number of L1 blocks. The synthetic configs use
// the same code paths and enforce the same protocol rules.
//
// All derivation tests require Fjord active (fjord_time = Some(0)) because
// the batcher always uses brotli compression, which the verifier's BatchReader
// rejects when Fjord is not active. Setting fjord_time = Some(0) activates
// the full cascade from Canyon through Fjord, so Delta is also active.
//
// When testing a hardfork boundary at a specific timestamp T, all forks
// *earlier* than that fork are set to Some(0) so they are active from
// genesis and do not activate again at T. Only the fork under test
// activates at T, keeping the upgrade-transaction injection isolated.
// ---------------------------------------------------------------------------

/// With only Canyon active (Delta NOT active, Fjord NOT active), a span batch
/// submitted by the batcher must NOT be derived.
///
/// Two protocol gates combine to prevent derivation:
/// 1. The batcher uses brotli compression; `BatchReader` rejects brotli before Fjord.
/// 2. Even without the compression gate, `SpanBatch` validation drops the batch
///    when Delta is not active at the batch L1 origin (`SpanBatchPreDelta`).
///
/// The net result: zero L2 blocks derived, pipeline returns `Ok(0)`.
#[tokio::test]
async fn span_batch_rejected_before_delta() {
    let batcher_cfg = BatcherConfig::default();
    let hardforks = HardForkConfig { canyon_time: Some(0), ..Default::default() };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let span_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..batcher_cfg.clone()
    };

    // Both blocks in one source, one advance produces a span batch.
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    source.push(builder.build_next_block_with_single_transaction().await);
    let mut batcher = Batcher::new(source, &h.rollup_config, span_cfg);
    batcher.advance(&mut h.l1).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(
        node.l2_safe_number(),
        0,
        "no blocks should be derived: span batch rejected pre-Delta/Fjord"
    );
}

/// With Fjord active from genesis (which cascades to activate Ecotone, Delta,
/// Canyon, and Regolith), a span batch derives successfully.
///
/// Fjord is required for two reasons: it activates Delta (via cascade), and
/// the batcher's brotli compression is only accepted by the verifier at Fjord.
#[tokio::test]
async fn span_batch_derives_after_delta() {
    let batcher_cfg = BatcherConfig::default();
    let hardforks = HardForkConfig { fjord_time: Some(0), ..Default::default() };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let span_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..batcher_cfg.clone()
    };

    // Both blocks in one source → one span batch in one L1 block.
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    source.push(builder.build_next_block_with_single_transaction().await);
    let mut batcher = Batcher::new(source, &h.rollup_config, span_cfg);
    batcher.advance(&mut h.l1).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 2, "expected 2 L2 blocks derived from span batch");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance to block 2");
}

/// Control test: with Fjord active, `SingleBatch` (the default encoding) derives
/// one L2 block per L1 block, regardless of span batch gating.
#[tokio::test]
async fn single_batch_derives_with_fjord() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let hardforks = HardForkConfig { fjord_time: Some(0), ..Default::default() };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_hardforks(hardforks).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 0..2 {
        batcher.push_block(builder.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 2, "both L2 blocks must be derived");
    assert_eq!(node.l2_safe_number(), 2, "safe head should advance to block 2");
}

/// Derivation must succeed across the Jovian activation boundary.
///
/// Configuration: all forks from Canyon through Isthmus are active from
/// genesis (`Some(0)`). Jovian activates at ts=6 (L2 block 3 with `block_time=2`).
/// Because each preceding fork is already active at genesis, the
/// `is_first_X_block` check returns false for all of them at ts=6 — only
/// `is_first_jovian_block` returns true. This isolates the upgrade-transaction
/// injection to Jovian alone, with no simultaneous cascade activations.
///
/// The first Jovian block (block 3) must be empty (no user transactions).
/// The batch validator enforces `NonEmptyTransitionBlock`: any batch containing
/// user transactions in the upgrade block is dropped, because the derivation
/// pipeline prepends the Jovian upgrade deposit transactions to that block's
/// attribute set.
#[tokio::test]
async fn jovian_derivation_crosses_activation_boundary() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    // All forks through Isthmus active from genesis so that at ts=6 only
    // Jovian is "new". With block_time=2 and L2 genesis at ts=0:
    //   block 1 → ts=2  (pre-Jovian, user txs ok)
    //   block 2 → ts=4  (pre-Jovian, user txs ok)
    //   block 3 → ts=6  (first Jovian block — must be empty)
    //   block 4 → ts=8  (post-Jovian, user txs ok)
    let jovian_time = 6u64;
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(jovian_time)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut block_hashes = [Default::default(); 5];
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for i in 1..=4u64 {
        let block = if i == 3 {
            // First Jovian block: must contain no user transactions.
            builder.build_empty_block().await
        } else {
            builder.build_next_block_with_single_transaction().await
        };
        block_hashes[i as usize] = block.header.hash_slow();
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    // The Jovian activation block (block 3) has upgrade deposit transactions
    // injected by the derivation pipeline that the sequencer did not execute.
    // These contract deployments change the EVM state root, causing a permanent
    // EVM state divergence from the sequencer for all subsequent blocks. Clear
    // the sequencer-registered state roots for blocks 3 and 4 so the node
    // skips state-root validation for them.
    node.register_block_hash(3, block_hashes[3]);
    node.register_block_hash(4, block_hashes[4]);
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 4, "all 4 L2 blocks must be derived");
    assert_eq!(
        node.l2_safe_number(),
        4,
        "safe head should advance past the Jovian activation boundary"
    );
}
