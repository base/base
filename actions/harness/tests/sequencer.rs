#![doc = "Action tests for the L2Sequencer actor."]

use base_action_harness::{
    Action, ActionL2ChainProvider, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig,
    L2BlockProvider, SharedL1Chain, block_info_from,
};
use base_consensus_genesis::{ChainGenesis, HardForkConfig, RollupConfig, SystemConfig};

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// `l2_block_time` sets the L2 block time; use 2 for most sequencer tests
/// so 6 L2 blocks fit inside a single 12-second L1 epoch.
fn rollup_config_for(batcher: &BatcherConfig, l2_block_time: u64) -> RollupConfig {
    RollupConfig {
        batch_inbox_address: batcher.inbox_address,
        block_time: l2_block_time,
        max_sequencer_drift: 600,
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

// ── Basic block production ─────────────────────────────────────────────────

/// Sequencer produces blocks with sequentially increasing numbers.
#[test]
fn sequencer_numbers_advance() {
    let cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    for expected in 1..=5u64 {
        seq.act().unwrap();
        assert_eq!(seq.unsafe_head().block_info.number, expected);
    }
}

/// Each L2 block's timestamp advances by `block_time` from the genesis time.
#[test]
fn sequencer_timestamps_advance_by_block_time() {
    let cfg = BatcherConfig::default();
    let block_time = 2u64;
    let rollup_cfg = rollup_config_for(&cfg, block_time);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    for n in 1..=6u64 {
        seq.act().unwrap();
        assert_eq!(
            seq.unsafe_head().block_info.timestamp,
            n * block_time,
            "block {n} has wrong timestamp"
        );
    }
}

/// Built blocks accumulate in the queue and are popped in FIFO order.
#[test]
fn sequencer_queued_blocks_drained_in_order() {
    let cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    seq.act().unwrap();
    seq.act().unwrap();
    seq.act().unwrap();
    assert_eq!(seq.queued_blocks(), 3);

    let b1 = seq.next_block().unwrap();
    let b2 = seq.next_block().unwrap();
    let b3 = seq.next_block().unwrap();
    assert!(seq.next_block().is_none());

    assert_eq!(b1.header.number, 1);
    assert_eq!(b2.header.number, 2);
    assert_eq!(b3.header.number, 3);
}

// ── Epoch selection ────────────────────────────────────────────────────────

/// While no new L1 block is available the epoch stays at genesis (L1 block 0).
#[test]
fn sequencer_epoch_stays_at_genesis_without_new_l1_block() {
    let cfg = BatcherConfig::default();
    // L1 block_time=12, L2 block_time=2: blocks 1-5 (t=2..10) all precede L1#1 (t=12).
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Only genesis is in the chain — no L1#1 to advance to.
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    for n in 1..=5u64 {
        seq.act().unwrap();
        assert_eq!(
            seq.unsafe_head().l1_origin.number,
            0,
            "block {n} should still be in epoch L1#0"
        );
    }
}

/// When L1 block 1 is added to the shared chain, the sequencer advances its
/// epoch once the next L2 block's timestamp reaches that L1 block's timestamp.
#[test]
fn sequencer_epoch_advances_when_l1_block_mined() {
    let cfg = BatcherConfig::default();
    // L1 block_time=12, L2 block_time=2.
    // L1#1 timestamp = 12.  L2 blocks 1-5 (t=2..10) stay on epoch 0.
    // L2 block 6 (t=12) satisfies t >= L1#1.timestamp → epoch advances to 1.
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 block 1 so the sequencer can advance the epoch.
    h.l1.mine_block();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    // L2 blocks 1-5: t=2,4,6,8,10 — all less than L1#1 t=12 → stay on epoch 0.
    for _ in 0..5 {
        seq.act().unwrap();
        assert_eq!(seq.unsafe_head().l1_origin.number, 0);
    }

    // L2 block 6: t=12 — L1#1 t=12 ≤ 12 → advance to epoch 1.
    seq.act().unwrap();
    assert_eq!(seq.unsafe_head().l1_origin.number, 1, "block 6 should be in epoch L1#1");
    assert_eq!(seq.unsafe_head().seq_num, 0, "seq_num resets on epoch advance");

    // L2 block 7: t=14 — no L1#2 → stay on epoch 1.
    seq.act().unwrap();
    assert_eq!(seq.unsafe_head().l1_origin.number, 1, "block 7 should remain in epoch L1#1");
    assert_eq!(seq.unsafe_head().seq_num, 1, "seq_num increments within epoch");
}

/// seq_num resets to 0 at the epoch boundary and increments within the epoch.
#[test]
fn sequencer_seq_num_resets_on_epoch_change() {
    let cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    h.l1.mine_block(); // L1#1 at t=12
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain);

    // Blocks 1-5 are in epoch 0; seq_num climbs.
    for _ in 0..5 {
        seq.act().unwrap();
    }
    assert_eq!(seq.unsafe_head().seq_num, 5);

    // Block 6 flips to epoch 1; seq_num resets.
    seq.act().unwrap();
    assert_eq!(seq.unsafe_head().l1_origin.number, 1);
    assert_eq!(seq.unsafe_head().seq_num, 0);

    // Blocks 7-8 stay in epoch 1; seq_num climbs again.
    seq.act().unwrap();
    assert_eq!(seq.unsafe_head().seq_num, 1);
    seq.act().unwrap();
    assert_eq!(seq.unsafe_head().seq_num, 2);
}

// ── End-to-end: sequencer → batcher → verifier ────────────────────────────

/// Full derivation pipeline: the sequencer builds L2 blocks, the batcher
/// encodes them, and the verifier re-derives the same L2 blocks from the
/// L1 chain.
#[tokio::test]
async fn sequencer_blocks_derivable_end_to_end() {
    let batcher_cfg = BatcherConfig::default();
    // L2 block_time=2, L1 block_time=12.
    // Build 5 L2 blocks (t=2..10); all are in epoch L1#0 (t=0) because no
    // L1#1 is mined yet when the sequencer runs.
    let rollup_cfg = rollup_config_for(&batcher_cfg, 2);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Create a shared chain snapshot and a sequencer wired to it.
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain.clone());

    // Build 5 L2 blocks (all in epoch L1#0).
    for _ in 0..5 {
        seq.act().unwrap();
    }
    assert_eq!(seq.queued_blocks(), 5);

    // Pass the sequencer as the L2 source directly to the batcher.
    // Batcher::new is generic over L2BlockProvider, so this works without
    // going through the harness's create_batcher helper.
    {
        let mut batcher = Batcher::new(&mut h.l1, seq, &h.rollup_config, batcher_cfg);
        batcher.advance().expect("batcher should encode 5 L2 blocks");
    }

    // Mine the L1 block that includes the batch transaction.
    h.l1.mine_block(); // L1#1 (contains batcher tx)
    chain.push(h.l1.tip().clone());

    // Create a fresh verifier wired to the same shared chain.
    let (mut verifier, _) =
        h.create_verifier_with_l2_provider(ActionL2ChainProvider::from_genesis(&h.rollup_config));

    verifier.initialize().await.unwrap();
    verifier.act_l1_head_signal(block_info_from(h.l1.block_by_number(1).unwrap())).await.unwrap();

    let derived = verifier.act_l2_pipeline_full().await.unwrap();
    assert_eq!(derived, 5, "expected 5 L2 blocks derived");
    assert_eq!(verifier.l2_safe().block_info.number, 5);
}

/// The sequencer can cross an epoch boundary: L2 blocks before the boundary
/// reference epoch L1#0, those at and after reference epoch L1#1. All blocks
/// are derivable by the verifier.
#[tokio::test]
async fn sequencer_cross_epoch_blocks_derivable() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg, 2);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1#1 so the sequencer can advance the epoch.
    h.l1.mine_block(); // L1#1 t=12
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq = h.create_sequencer(chain.clone());

    // Build L2 blocks 1-6.  Blocks 1-5 (t=2..10) stay in epoch L1#0.
    // Block 6 (t=12) flips to epoch L1#1.
    for _ in 0..6 {
        seq.act().unwrap();
    }
    assert_eq!(seq.unsafe_head().block_info.number, 6);
    assert_eq!(seq.unsafe_head().l1_origin.number, 1);

    // Encode and submit blocks 1-6, then mine L1#2 (the batch-carrying block).
    {
        let mut batcher = Batcher::new(&mut h.l1, seq, &h.rollup_config, batcher_cfg);
        batcher.advance().unwrap();
    }
    h.l1.mine_block(); // L1#2
    chain.push(h.l1.tip().clone());

    // Derive: verifier needs to see up to L1#2.
    let (mut verifier, _) =
        h.create_verifier_with_l2_provider(ActionL2ChainProvider::from_genesis(&h.rollup_config));

    verifier.initialize().await.unwrap();
    verifier.act_l1_head_signal(block_info_from(h.l1.block_by_number(1).unwrap())).await.unwrap();
    verifier.act_l2_pipeline_full().await.unwrap();
    verifier.act_l1_head_signal(block_info_from(h.l1.block_by_number(2).unwrap())).await.unwrap();
    let derived = verifier.act_l2_pipeline_full().await.unwrap();

    assert!(derived > 0, "expected at least one L2 block derived on L1#2");
    assert_eq!(verifier.l2_safe().block_info.number, 6, "all 6 L2 blocks should be derived");
}

/// After an L1 reorg, L2 blocks that advance to the replaced L1 block carry
/// a different epoch hash than blocks from the original fork.
///
/// L2 block 6 (t=12) crosses the epoch boundary to L1#1. On the original fork
/// L1#1 has hash A; after a reorg it has hash B. The two sequencers therefore
/// produce L2 block 6 with different epoch hashes.
#[test]
fn sequencer_reorg_produces_distinct_epoch_hash() {
    let cfg = BatcherConfig::default();
    // L1 block_time=12, L2 block_time=2.
    // L2 block 6 (t=12) is the first to advance past L1#1 (t=12).
    let rollup_cfg = rollup_config_for(&cfg, 2);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // ── Fork A ─────────────────────────────────────────────────────────────
    h.l1.mine_block(); // L1#1 on fork A
    let l1_1_hash_a = h.l1.tip().hash();

    let chain_a = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq_a = h.create_sequencer(chain_a);
    for _ in 0..6 {
        seq_a.act().unwrap();
    }
    let head_6_a = seq_a.unsafe_head();
    assert_eq!(head_6_a.block_info.number, 6);
    assert_eq!(head_6_a.l1_origin.number, 1);
    assert_eq!(head_6_a.l1_origin.hash, l1_1_hash_a);

    // ── Fork B (after reorg) ────────────────────────────────────────────────
    h.l1.reorg_to(0).unwrap(); // discard L1#1 fork A
    h.l1.mine_block(); // L1#1' on fork B (different hash due to fork_id)
    let l1_1_hash_b = h.l1.tip().hash();
    assert_ne!(l1_1_hash_a, l1_1_hash_b, "fork_id must make L1#1' distinct");

    let chain_b = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq_b = h.create_sequencer(chain_b);
    for _ in 0..6 {
        seq_b.act().unwrap();
    }
    let head_6_b = seq_b.unsafe_head();
    assert_eq!(head_6_b.block_info.number, 6);
    assert_eq!(head_6_b.l1_origin.number, 1);
    assert_eq!(head_6_b.l1_origin.hash, l1_1_hash_b);

    // Same L2 block number, different epoch hashes — the reorg is visible.
    assert_eq!(head_6_a.block_info.number, head_6_b.block_info.number);
    assert_ne!(
        head_6_a.l1_origin.hash,
        head_6_b.l1_origin.hash,
        "L2 block 6 on different L1 forks must reference different epoch hashes"
    );
}
