#![doc = "TDD action test skeletons for sequencer drift scenarios."]

use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, block_info_from,
};
use base_consensus_genesis::{ChainGenesis, HardForkConfig, RollupConfig, SystemConfig};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a [`RollupConfig`] with tight `max_sequencer_drift` for drift tests.
///
/// - `block_time = 2` (L2 block interval)
/// - `max_sequencer_drift = 8` (4 L2 blocks before drift is exceeded)
/// - L1 `block_time = 4` (via [`L1MinerConfig`])
/// - All other windows are generous so only drift matters.
fn drift_rollup_config(batcher: &BatcherConfig) -> RollupConfig {
    RollupConfig {
        batch_inbox_address: batcher.inbox_address,
        block_time: 2,
        max_sequencer_drift: 8,
        seq_window_size: 3600,
        channel_timeout: 300,
        genesis: ChainGenesis {
            system_config: Some(SystemConfig {
                batcher_address: batcher.batcher_address,
                gas_limit: 30_000_000,
                ..Default::default()
            }),
            ..Default::default()
        },
        hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// A. Sequencer drift — L2 timestamp exceeds L1 origin time + max_sequencer_drift
// ---------------------------------------------------------------------------

/// When the L2 sequencer is pinned to a stale L1 origin and builds enough
/// blocks that `L2_timestamp > L1_origin_time + max_sequencer_drift`, the
/// derivation pipeline should still derive those blocks — but any non-empty
/// batch (one containing user transactions) whose timestamp is past the drift
/// boundary is dropped. Only deposit-only (default) blocks are produced for
/// the over-drift slots.
///
/// This is the Rust analogue of op-e2e's `MaxSequencerDriftFnAfterDelta` test.
///
/// ## Setup
///
/// - `max_sequencer_drift = 8`, `block_time = 2`, L1 `block_time = 4`
/// - L1 genesis at ts=0 → L1 block 1 at ts=4
/// - Pin the sequencer to L1 genesis (epoch 0, ts=0)
/// - Build L2 blocks: ts=2, 4, 6, 8, 10, ...
/// - After L2 block 5 (ts=10), `10 > 0 + 8 = 8` → drift exceeded
///
/// ## Expected behaviour
///
/// The derivation pipeline:
/// 1. Accepts L2 blocks 1-4 (timestamps 2-8, within drift) as submitted
/// 2. For L2 block 5+ (timestamps 10+, over drift), drops the batcher's
///    non-empty batch and generates deposit-only default blocks instead
///
/// ## Harness requirements
///
/// This test needs:
/// - `L2Sequencer::pin_l1_origin()` to prevent epoch advance (ALREADY EXISTS)
/// - `L2Sequencer::build_empty_block()` for forced-empty blocks (ALREADY EXISTS)
/// - The sequencer to allow building blocks past the drift boundary. Currently
///   `build_next_block` does NOT enforce max_sequencer_drift — it builds any
///   block the caller requests. The enforcement happens in the derivation
///   pipeline's `BatchQueue`, which is what we are testing here.
#[tokio::test]
#[ignore = "requires verifying deposit-only block content from pipeline attributes; \
            the pipeline may generate default blocks but we need to assert they contain \
            no user transactions — currently no assertion API for inspecting derived \
            attributes' transaction content"]
async fn sequencer_drift_produces_deposit_only_blocks() {
    let l1_cfg = L1MinerConfig { block_time: 4 };
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = drift_rollup_config(&batcher_cfg);
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg.clone());

    // Mine L1 block 1 (ts=4) so the sequencer has an epoch to reference,
    // but we will PIN the sequencer to epoch 0 (ts=0) to force drift.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Pin the sequencer to L1 genesis (epoch 0, ts=0).
    let l1_genesis = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    sequencer.pin_l1_origin(l1_genesis);

    // Build 6 L2 blocks pinned to epoch 0:
    //   block 1: ts=2  (drift = 2-0 = 2 <= 8) ✓
    //   block 2: ts=4  (drift = 4-0 = 4 <= 8) ✓
    //   block 3: ts=6  (drift = 6-0 = 6 <= 8) ✓
    //   block 4: ts=8  (drift = 8-0 = 8 <= 8) ✓
    //   block 5: ts=10 (drift = 10-0 = 10 > 8) ✗ over drift
    //   block 6: ts=12 (drift = 12-0 = 12 > 8) ✗ over drift
    //
    // Blocks 1-4 have user transactions. Blocks 5-6 also have user txs
    // (sequencer doesn't enforce drift), but the pipeline should drop them.
    let (mut verifier, chain) = h.create_verifier();
    let mut block_hashes: Vec<(u64, B256)> = Vec::new();

    for i in 1u64..=6 {
        // Build with user transactions — the pipeline decides what to accept.
        let block = sequencer.build_next_block().expect("build L2 block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        // Submit each block as a separate batch in its own L1 block.
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher encode");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    // Drive derivation through all L1 blocks.
    let mut total_derived = 0;
    for i in 1..=h.l1.latest_number() {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    // The pipeline should derive blocks for all L2 slots. Blocks 1-4 use the
    // batcher's submitted batches. Blocks 5-6 are generated as deposit-only
    // default blocks because the non-empty batches are dropped for exceeding
    // max_sequencer_drift.
    assert!(
        verifier.l2_safe().block_info.number >= 4,
        "at least 4 L2 blocks should be derived (within drift)"
    );
    assert!(total_derived >= 4, "at least 4 blocks derived");

    // TODO: Assert that the derived attributes for blocks 5-6 contain no
    // user transactions (deposit-only). This requires either:
    // (a) The verifier to expose the OpAttributesWithParent for each derived block, or
    // (b) A callback/hook in apply_attributes that records transaction counts.
    //
    // Harness extension needed:
    //   pub fn derived_attributes_history(&self) -> &[OpAttributesWithParent]
    // or:
    //   pub fn derived_tx_counts(&self) -> &[(u64, usize)]  // (block_number, tx_count)
}

// ---------------------------------------------------------------------------
// B. Sequencer drift with forced-empty blocks
// ---------------------------------------------------------------------------

/// When max_sequencer_drift is exceeded, the sequencer should produce
/// deposit-only (empty) blocks. These empty blocks should be accepted by
/// the derivation pipeline because they contain no user transactions.
///
/// This test uses `L2Sequencer::build_empty_block()` for the over-drift
/// blocks, confirming that the pipeline accepts forced-empty batches even
/// past the drift boundary.
#[tokio::test]
#[ignore = "requires verifying that the pipeline accepts empty batches past \
            max_sequencer_drift; the L2Sequencer supports build_empty_block() \
            but the derivation pipeline's handling of empty batches at the drift \
            boundary needs end-to-end verification in the harness"]
async fn sequencer_drift_forced_empty_blocks_accepted() {
    let l1_cfg = L1MinerConfig { block_time: 4 };
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = drift_rollup_config(&batcher_cfg);
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg);

    // Mine 1 L1 block so epoch 1 exists, but pin sequencer to epoch 0.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let l1_genesis = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    sequencer.pin_l1_origin(l1_genesis);

    let (mut verifier, chain) = h.create_verifier();
    let mut block_hashes: Vec<(u64, B256)> = Vec::new();

    // Build 4 normal blocks (within drift) + 2 empty blocks (over drift).
    for i in 1u64..=4 {
        let block = sequencer.build_next_block().expect("build normal block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    // Build empty blocks past the drift boundary.
    for i in 5u64..=6 {
        let block = sequencer.build_empty_block().expect("build empty block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        // The empty block has only the deposit tx — the batcher should still
        // encode it (the batch contains epoch info but no user txs).
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode empty block");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    let mut total_derived = 0;
    for i in 1..=h.l1.latest_number() {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    // All 6 blocks should be derived: 4 normal + 2 empty.
    // The empty blocks past the drift boundary should be accepted because
    // they contain no user transactions.
    assert!(
        total_derived >= 4,
        "at least the 4 within-drift blocks should be derived"
    );
    assert_eq!(
        verifier.l2_safe().block_info.number,
        total_derived as u64,
        "safe head should match number of derived blocks"
    );
}
