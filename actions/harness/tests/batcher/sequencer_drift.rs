//! Sequencer drift: past `max_sequencer_drift`, derivation drops batches with user
//! transactions, and empty batches that keep a stale origin while a next one exists.
//! Deposit-only blocks fill those slots only once the sequencing window closes.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Past `max_sequencer_drift`, derivation drops the batches with user transactions and,
/// once epoch 0's sequencing window closes, fills their slots with deposit-only blocks.
///
/// Fjord sets `max_sequencer_drift` to 1800 s. With an L2 block time of 300 s and the
/// sequencer pinned to L1 genesis (time 0), blocks 1 to 6 are within drift and blocks 7 and
/// 8 are past it. A sequencing window of 2 L1 blocks, with the batch in L1 block 2, closes at
/// L1 block 3: L1 blocks 3 and 4 each yield one deposit-only block.
#[tokio::test]
async fn over_drift_batches_with_transactions_become_deposit_only_once_the_window_closes() {
    let l1_cfg = L1MinerConfig { block_time: 4, ..Default::default() };
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_block_time(300)
        // Small sequence window so the pipeline generates deposit-only blocks for the
        // over-drift slots once the window expires. With seq_window_size=2 and the batch
        // submitted in L1 block 2, the window expires at L1 block 3 (epoch 0 + 2 < 3),
        // prompting the pipeline to auto-generate default blocks for slots 7 and 8.
        .with_seq_window_size(2)
        .build();
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg.clone());

    // Mine L1 block 1 (ts=4) so the sequencer has an epoch to reference,
    // but we will PIN the sequencer to epoch 0 (ts=0) to force drift.
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Pin the sequencer to L1 genesis (epoch 0, ts=0).
    let l1_genesis = h.l1.block_info_at(0);
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

    // Collect all 8 blocks and batch them in one L1 block.
    let mut source = ActionL2Source::new();
    for _ in 1u64..=8 {
        source.push(sequencer.build_next_block_with_single_transaction().await);
    }

    // Create the node from a separate sequencer that has an empty block-hash
    // registry. Blocks 7-8 are dropped by the pipeline (over-drift) and
    // replaced with deposit-only default blocks whose state roots differ from
    // the sequencer's. Using a fresh sequencer avoids a false state-root
    // mismatch for those slots.
    let mut node_sequencer =
        h.create_l2_sequencer(SharedL1Chain::from_blocks(h.l1.chain().to_vec()));
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut node_sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    let batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    // Mine 2 extra empty L1 blocks (seq_window_size=2, batch epoch=0, batch
    // in L1 block 2 → window expires at L1 block 3). The pipeline needs to
    // see L1 blocks 3 and 4 to auto-generate deposit-only blocks for slots 7-8:
    // block 3 produces slot 7, block 4 produces slot 8.
    h.mine_and_push(&chain);
    h.mine_and_push(&chain);

    node.initialize().await;

    // Drive derivation through all L1 blocks.
    let mut total_derived = 0;
    for _ in 1..=h.l1.latest_number() {
        total_derived += node.run_until_idle().await;
    }

    // The pipeline should derive blocks for all L2 slots. Blocks 1-6 use the
    // batcher's submitted batches. Blocks 7-8 are generated as deposit-only
    // default blocks because the non-empty batches are dropped for exceeding
    // max_sequencer_drift.
    assert_eq!(
        node.l2_safe_number(),
        8,
        "all 8 L2 blocks must be derived (blocks 7-8 as deposit-only over-drift blocks)"
    );
    assert_eq!(total_derived, 8, "all 8 blocks derived");

    // Blocks 1-6 keep their user transaction, blocks 7-8 are deposit-only.
    for number in 1..=8 {
        let block = node.derived_block(number).expect("derived block");
        assert_eq!(block.is_deposit_only(), number > 6, "block {number}");
    }
}

/// Past `max_sequencer_drift`, a batch that keeps the stale L1 origin while a next origin
/// whose timestamp the batch has reached exists is dropped, even an empty one: derivation
/// stops at the last block within drift and does not fill the dropped slots with deposit-only
/// blocks while epoch 0's sequencing window is open.
#[tokio::test]
async fn over_drift_empty_batches_are_dropped_when_a_next_origin_exists() {
    let l1_cfg = L1MinerConfig { block_time: 4, ..Default::default() };
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
    let l1_genesis = h.l1.block_info_at(0);
    sequencer.pin_l1_origin(l1_genesis);

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Build 6 normal blocks (within drift, ts=300..1800) + 2 empty blocks
    // (over drift, ts=2100, 2400). block_time=300 s, max_drift=1800 s.
    let mut source = ActionL2Source::new();
    for _ in 1u64..=6 {
        source.push(sequencer.build_next_block_with_single_transaction().await);
    }
    // Empty blocks past the drift boundary, still on epoch 0 although L1 block 1 exists.
    for _ in 7u64..=8 {
        source.push(sequencer.build_empty_block().await);
    }

    let batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;

    let mut total_derived = 0;
    for _ in 1..=h.l1.latest_number() {
        total_derived += node.run_until_idle().await;
    }

    assert_eq!(total_derived, 6, "the over-drift batches are dropped");
    assert_eq!(node.l2_safe_number(), 6);
}
