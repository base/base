#![doc = "Action tests for L2 derivation via the verifier pipeline."]

use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, MockL2Block, block_info_from,
};
use base_consensus_genesis::{ChainGenesis, HardForkConfig, RollupConfig, SystemConfig};

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// The config is minimal but sufficient for derivation tests:
/// - `batch_inbox_address` matches the batcher's inbox so frames are picked up.
/// - `genesis.system_config.batcher_address` matches the batcher's address so
///   the pipeline's batcher-address filter accepts the L1 transactions.
/// - `block_time = 2` so L2 timestamps advance.
/// - `seq_window_size` and `channel_timeout` are generous so no windows expire.
fn rollup_config_for(batcher: &BatcherConfig) -> RollupConfig {
    RollupConfig {
        batch_inbox_address: batcher.inbox_address,
        block_time: 2,
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
        // The batcher uses brotli compression which BatchReader only accepts
        // when Fjord is active.  Setting fjord_time=0 also implicitly activates
        // all earlier hardforks (canyon/delta/ecotone) via the cascading
        // is_X_active checks, so no transition blocks are triggered.
        hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
        ..Default::default()
    }
}

/// The derivation pipeline reads a single batcher frame from L1 and derives
/// the corresponding L2 block, advancing the safe head from genesis (0) to 1.
#[tokio::test]
async fn single_l2_block_derived_from_batcher_frame() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // The epoch hash for the mock L2 block must match the actual L1 genesis hash
    // so that the batch epoch-hash validation passes.
    let l1_genesis_hash = h.l1.chain()[0].hash();

    // Push L2 block 1: first block after genesis, building on L1 genesis (epoch 0).
    //
    // - parent_hash = B256::ZERO (L2 genesis hash; default ChainGenesis.l2.hash)
    // - timestamp   = 2         (genesis l2_time=0 + block_time=2)
    // - epoch_num   = 0         (L1 genesis epoch)
    // - epoch_hash  = actual L1 genesis hash (validated by pipeline)
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });

    // Encode the L2 block into a batcher frame and submit to the L1 pending pool.
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("batcher should encode the block");
    drop(batcher);

    // Mine the L1 block that includes the batcher transaction.
    h.l1.mine_block();

    // Create the verifier AFTER mining so the SharedL1Chain snapshot already
    // contains both genesis and block 1.
    let (mut verifier, _chain) = h.create_verifier();
    let l1_block_1 = block_info_from(h.l1.tip());

    // Seed the genesis SystemConfig (batcher address) and drain the empty
    // genesis L1 block so IndexedTraversal is ready for new block signals.
    verifier.initialize().await.expect("initialize should succeed");

    // Signal the new L1 head. IndexedTraversal validates the parent-hash chain,
    // so this only succeeds because block 1's parent_hash equals genesis.hash().
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal should succeed");

    // Step the pipeline until it is idle.
    let derived = verifier.act_l2_pipeline_full().await.expect("pipeline step should succeed");

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should be L2 block 1");
}

/// Mine several L1 blocks, each containing one batch, and verify the safe head
/// advances by one L2 block per L1 block.
///
/// All three L2 blocks belong to the same L1 epoch (genesis). This is the
/// realistic Optimism scenario: with 12 s L1 blocks and 2 s L2 blocks there
/// are ~6 L2 slots per L1 epoch; each batch may land in a different L1 block
/// within the sequencer window while still referencing the same L1 epoch.
#[tokio::test]
async fn multiple_l1_blocks_each_derive_one_l2_block() {
    const L2_BLOCK_COUNT: u64 = 3;

    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // All L2 blocks reference L1 genesis (epoch 0, ts=0) as their origin.
    // Their timestamps (2, 4, 6 s) satisfy `batch.ts >= epoch.ts` (0 s).
    let l1_genesis_hash = h.l1.chain()[0].hash();

    for i in 1..=L2_BLOCK_COUNT {
        h.push_l2_block(MockL2Block {
            number: i,
            timestamp: i * 2,
            l1_origin_number: 0,
            l1_origin_hash: l1_genesis_hash,
            ..Default::default()
        });

        let mut batcher = h.create_batcher(batcher_cfg.clone());
        batcher.advance().expect("batcher advance");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize should succeed");

    // Drive derivation one L1 block at a time.
    for i in 1..=L2_BLOCK_COUNT {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal ok");
        let derived = verifier.act_l2_pipeline_full().await.expect("pipeline ok");
        assert_eq!(derived, 1, "L1 block {} should derive exactly one L2 block", i);
    }

    assert_eq!(verifier.l2_safe().block_info.number, L2_BLOCK_COUNT);
}
