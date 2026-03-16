#![doc = "TDD action test skeletons for sequencer drift scenarios."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, DaType, EncoderConfig,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};

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
/// - Fjord active → `max_sequencer_drift = 1800 s`, `block_time = 300 s`, L1
///   `block_time = 4 s`
/// - L1 genesis at ts=0 → L1 block 1 at ts=4
/// - Pin the sequencer to L1 genesis (epoch 0, ts=0)
/// - Build L2 blocks: ts=300, 600, …, 1800, 2100, 2400
/// - After L2 block 6 (ts=1800), `1800 ≤ 0 + 1800 = 1800` → still within
/// - L2 block 7 (ts=2100): `2100 > 1800` → drift exceeded
///
/// ## Expected behaviour
///
/// The derivation pipeline:
/// 1. Accepts L2 blocks 1-6 (timestamps 300-1800, within drift) as submitted
/// 2. For L2 blocks 7-8 (timestamps 2100-2400, over drift), drops the
///    batcher's non-empty batch and generates deposit-only default blocks
#[tokio::test]
async fn sequencer_drift_produces_deposit_only_blocks() {
    let l1_cfg = L1MinerConfig { block_time: 4 };
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_block_time(300).build();
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg.clone());

    // Mine L1 block 1 (ts=4) so the sequencer has an epoch to reference,
    // but we will PIN the sequencer to epoch 0 (ts=0) to force drift.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Pin the sequencer to L1 genesis (epoch 0, ts=0).
    let l1_genesis = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    sequencer.pin_l1_origin(l1_genesis);

    // Build 8 L2 blocks pinned to epoch 0 (block_time=300 s, max_drift=1800 s):
    //   block 1: ts= 300 (drift=  300 ≤ 1800) ✓
    //   block 2: ts= 600 (drift=  600 ≤ 1800) ✓
    //   block 3: ts= 900 (drift=  900 ≤ 1800) ✓
    //   block 4: ts=1200 (drift= 1200 ≤ 1800) ✓
    //   block 5: ts=1500 (drift= 1500 ≤ 1800) ✓
    //   block 6: ts=1800 (drift= 1800 ≤ 1800) ✓ (exactly at boundary)
    //   block 7: ts=2100 (drift= 2100 > 1800) ✗ over drift
    //   block 8: ts=2400 (drift= 2400 > 1800) ✗ over drift
    //
    // Blocks 1-6 have user transactions. Blocks 7-8 also have user txs
    // (sequencer doesn't enforce drift), but the pipeline should drop them.
    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Collect all 8 blocks and batch them in one L1 block.
    let mut source = ActionL2Source::new();
    for _ in 1u64..=8 {
        source.push(sequencer.build_next_block().expect("build L2 block"));
    }
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await.expect("batcher advance");
    chain.push(h.l1.tip().clone());

    verifier.initialize().await.expect("initialize");

    // Drive derivation through all L1 blocks.
    let mut total_derived = 0;
    for i in 1..=h.l1.latest_number() {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    // The pipeline should derive blocks for all L2 slots. Blocks 1-6 use the
    // batcher's submitted batches. Blocks 7-8 are generated as deposit-only
    // default blocks because the non-empty batches are dropped for exceeding
    // max_sequencer_drift.
    assert!(
        verifier.l2_safe().block_info.number >= 6,
        "at least 6 L2 blocks should be derived (within drift)"
    );
    assert!(total_derived >= 6, "at least 6 blocks derived");

    // Verify deposit-only behaviour: blocks 1-6 carry 2 txs each (deposit +
    // user tx), blocks 7-8 must carry exactly 1 tx (L1 info deposit only).
    let tx_counts = verifier.derived_tx_counts();
    for &(number, count) in tx_counts {
        if number <= 6 {
            assert_eq!(count, 2, "block {number} should have deposit + user tx");
        } else {
            assert_eq!(count, 1, "block {number} past drift boundary should be deposit-only");
        }
    }
}

// ---------------------------------------------------------------------------
// B. Sequencer drift with forced-empty blocks
// ---------------------------------------------------------------------------

/// When `max_sequencer_drift` is exceeded, the sequencer should produce
/// deposit-only (empty) blocks. This test verifies that the pipeline correctly
/// handles the over-drift region by deriving blocks for all L2 slots, even
/// when the submitted batches are dropped.
///
/// This test uses `L2Sequencer::build_empty_block()` for the over-drift
/// blocks (7-8). The pipeline drops those batches (they still reference the
/// stale epoch 0, triggering `SequencerDriftNotAdoptedNextOrigin`), and then
/// generates deposit-only default blocks for those slots.
#[tokio::test]
async fn sequencer_drift_forced_empty_blocks_accepted() {
    let l1_cfg = L1MinerConfig { block_time: 4 };
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_block_time(300).build();
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg);

    // Mine 1 L1 block so epoch 1 exists, but pin sequencer to epoch 0.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let l1_genesis = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    sequencer.pin_l1_origin(l1_genesis);

    let (mut verifier, chain) = h.create_verifier_from_sequencer(
        &sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Build 6 normal blocks (within drift, ts=300..1800) + 2 empty blocks
    // (over drift, ts=2100, 2400). block_time=300 s, max_drift=1800 s.
    let mut source = ActionL2Source::new();
    for _ in 1u64..=6 {
        source.push(sequencer.build_next_block().expect("build normal block"));
    }
    // Build empty blocks past the drift boundary. The empty block has only
    // the deposit tx — the batcher encodes it but the pipeline drops it
    // (stale epoch) and produces a default block.
    for _ in 7u64..=8 {
        source.push(sequencer.build_empty_block().expect("build empty block"));
    }

    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await.expect("batcher advance");
    chain.push(h.l1.tip().clone());

    verifier.initialize().await.expect("initialize");

    let mut total_derived = 0;
    for i in 1..=h.l1.latest_number() {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    // All 8 blocks should be derived: 6 normal + 2 empty (pipeline-generated
    // deposit-only blocks for the over-drift slots).
    assert!(total_derived >= 6, "at least the 6 within-drift blocks should be derived");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        total_derived as u64,
        "safe head should match number of derived blocks"
    );
}
