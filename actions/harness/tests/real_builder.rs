//! End-to-end action test: the production `SequencerActor` drives the real Flashblocks builder.
//!
//! Unlike the default action-harness sequencer (which assembles blocks with reth's execution-side
//! `BasePayloadBuilder` and a no-op pool), this drives an [`L2Sequencer`] backed by
//! [`BuilderBackedEngineClient`] — an in-process node running the production
//! `FlashblocksServiceBuilder`. It proves the harness's production sequencer actor can build blocks
//! through the real builder over the Engine API, against the harness's rollup-derived genesis.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, BuilderBackedEngineClient,
    L1MinerConfig, L2Sequencer, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Build a harness + builder-backed sequencer anchored a few seconds ahead of wall-clock, so the
/// Flashblocks builder schedules flashblocks and selects from the pool (see the module docs).
///
/// Returns the harness, the builder-backed sequencer, and the batcher config used to derive the
/// rollup config (needed to batch produced blocks back to L1).
async fn wall_clock_builder_sequencer()
-> eyre::Result<(ActionTestHarness, L2Sequencer<BuilderBackedEngineClient>, BatcherConfig)> {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let base_ts =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 4;
    let l1_cfg = L1MinerConfig { genesis_timestamp: base_ts, ..L1MinerConfig::default() };
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .build();
    rollup_cfg.genesis.l2_time = base_ts;
    let h = ActionTestHarness::new(l1_cfg, rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let sequencer = h.create_l2_sequencer_with_builder(l1_chain).await?;
    Ok((h, sequencer, batcher_cfg))
}

/// The production `SequencerActor`, driving the real builder backend, produces the first L2 block on
/// top of the harness genesis and advances the unsafe head.
#[tokio::test(flavor = "multi_thread")]
async fn builder_backed_sequencer_produces_block_through_actor() -> eyre::Result<()> {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    // Jovian active at genesis: the builder emits Jovian payload attributes (min_base_fee).
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer_with_builder(l1_chain).await?;

    let block = sequencer.build_empty_block().await;

    assert_eq!(block.header.number, 1, "actor + real builder must produce block 1");
    assert_eq!(
        block.header.parent_hash, h.rollup_config.genesis.l2.hash,
        "block 1 must build on the harness-derived genesis",
    );
    assert_eq!(sequencer.head().block_info.number, 1, "unsafe head must advance to block 1");

    Ok(())
}

/// With Base Azul active at genesis, sealing a payload must use `engine_getPayloadV5` (still
/// importing via `newPayloadV4`) rather than unconditionally calling `getPayloadV4`. Regression
/// test for a version mismatch that made builder-backed Azul/Beryl/Cobalt tests fail.
#[tokio::test(flavor = "multi_thread")]
async fn builder_backed_sequencer_produces_block_with_azul_active() -> eyre::Result<()> {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .with_azul_at(0)
        .build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer_with_builder(l1_chain).await?;

    let block = sequencer.build_empty_block().await;

    assert_eq!(block.header.number, 1, "actor + real builder must produce block 1 under Azul");
    assert_eq!(sequencer.head().block_info.number, 1, "unsafe head must advance to block 1");

    Ok(())
}

/// A harness-supplied user transaction is routed through the real mempool (not force-included) and
/// selected by the production builder into the block — exercising real pool-based tx selection.
///
/// The production Flashblocks builder schedules flashblocks from wall-clock time
/// (`calculate_flashblocks` produces none when the block timestamp is behind `now`). This test uses
/// the harness's wall-clock timestamp mode — anchoring L1 and L2 genesis a few seconds ahead of
/// `now` (well within `max_sequencer_drift`) — so the builder allocates flashblocks and pulls the
/// injected transaction from the pool.
#[tokio::test(flavor = "multi_thread")]
async fn builder_backed_sequencer_selects_pool_transaction() -> eyre::Result<()> {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    // Anchor genesis a little ahead of wall-clock so produced-block timestamps are in the future
    // (which the Flashblocks builder needs to schedule flashblocks), but close enough that the
    // block's slot lands well inside the harness's inserted-block timeout.
    let base_ts =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 4;
    let l1_cfg = L1MinerConfig { genesis_timestamp: base_ts, ..L1MinerConfig::default() };
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .build();
    rollup_cfg.genesis.l2_time = base_ts;
    let h = ActionTestHarness::new(l1_cfg, rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer_with_builder(l1_chain).await?;

    // In production-builder mode this transaction is injected into the real pool and selected by
    // the builder, rather than force-included via the payload attributes.
    let block = sequencer.build_next_block_with_single_transaction().await;

    assert_eq!(block.header.number, 1, "block 1 must be produced");
    // The block must contain the L1-info deposit plus the pool-selected user transaction.
    assert!(
        block.body.transactions.len() >= 2,
        "expected the pool-selected user tx alongside the L1-info deposit, got {} tx(s)",
        block.body.transactions.len(),
    );

    Ok(())
}

/// A block produced by the real builder is batched to L1 and re-derived by the verifier. The
/// verifier re-executes the derived block and — via the block-hash registry shared with the builder
/// backend — asserts its state root matches the builder-produced one, validating full
/// builder → batcher → derivation round-trip parity.
///
/// Uses the harness's normal (deterministic, ancient-timestamp) model: the real builder's
/// deposit-only fallback block is a valid production-built block. Pool-based selection is covered
/// separately by `builder_backed_sequencer_selects_pool_transaction`.
#[tokio::test(flavor = "multi_thread")]
async fn builder_block_round_trips_through_derivation() -> eyre::Result<()> {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer_with_builder(l1_chain).await?;

    // Build block 1 through the real builder (deposit-only fallback block under ancient timestamps).
    let block = sequencer.build_empty_block().await;
    assert_eq!(block.header.number, 1, "block 1 must be produced by the real builder");

    // Batch block 1 to L1.
    let mut source = ActionL2Source::new();
    source.push(block);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Derive with a verifier that shares the builder's block-hash registry: re-execution asserts
    // the derived block's state root equals the builder-produced one.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "exactly one L2 block should be derived");
    assert_eq!(node.l2_safe_number(), 1, "safe head should be L2 block 1");

    Ok(())
}
