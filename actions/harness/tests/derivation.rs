#![doc = "Action tests for L2 derivation via the verifier pipeline."]

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, LogData};
use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, MockL2Block, block_info_from,
};
use base_consensus_genesis::{
    CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC, ChainGenesis, HardForkConfig, RollupConfig,
    SystemConfig,
};
use base_protocol::{BlockInfo, L2BlockInfo};

/// Build an [`L2BlockInfo`] representing the L2 genesis state for a rollup.
///
/// The L2 genesis anchors to the given `l1_genesis` block as its L1 origin.
/// Used to populate [`L2Verifier::act_reset`] when reverting to genesis after
/// an L1 reorg.
fn l2_genesis_for(rollup_cfg: &RollupConfig, l1_genesis: BlockInfo) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo {
            hash: rollup_cfg.genesis.l2.hash,
            number: rollup_cfg.genesis.l2.number,
            parent_hash: Default::default(),
            timestamp: rollup_cfg.genesis.l2_time,
        },
        l1_origin: BlockNumHash { hash: l1_genesis.hash, number: l1_genesis.number },
        seq_num: 0,
    }
}

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
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(verifier.l2_safe().block_info.number, L2_BLOCK_COUNT);
}

/// A batcher frame that lands in an L1 block which is subsequently reorged out
/// must NOT be derived. The verifier is created on the post-reorg chain
/// (verifier never saw the orphaned block), so no reset is needed — the chain
/// snapshot passed to the verifier already reflects the canonical fork.
#[tokio::test]
async fn batch_in_orphaned_l1_block_is_not_derived() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_genesis_hash = h.l1.chain()[0].hash();

    // Encode L2 block 1 and mine L1 block 1 containing the batcher frame.
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("batcher encode");
    drop(batcher);
    h.l1.mine_block();

    // Reorg L1 back to genesis; mine an empty replacement block 1'.
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block();
    let l1_block_1_prime = block_info_from(h.l1.tip());

    // The verifier is created from the miner's current (post-reorg) state, so
    // the orphaned block 1 is not present in the SharedL1Chain snapshot.
    let (mut verifier, _chain) = h.create_verifier();

    verifier.initialize().await.expect("initialize");
    verifier.act_l1_head_signal(l1_block_1_prime).await.expect("signal block 1'");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");

    assert_eq!(derived, 0, "batch was in orphaned block; nothing should be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 0, "safe head remains at genesis");
}

/// After the verifier has derived L2 block 1 (safe head = 1), an L1 reorg
/// back to genesis is detected and the pipeline is reset. The safe head must
/// revert to 0 and no new L2 blocks must be derived from the empty replacement
/// L1 block.
#[tokio::test]
async fn reorg_reverts_derived_safe_head() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_genesis_hash = h.l1.chain()[0].hash();

    // Batch and mine L1 block 1.
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("batcher encode");
    drop(batcher);
    h.l1.mine_block();

    // Create the verifier and derive L2 block 1.
    let (mut verifier, chain) = h.create_verifier();
    let l1_block_1 = block_info_from(h.l1.tip());

    verifier.initialize().await.expect("initialize");
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");
    assert_eq!(derived, 1, "L2 block 1 derived before reorg");
    assert_eq!(verifier.l2_safe().block_info.number, 1);

    // Reorg L1 back to genesis; mine an empty replacement block 1'.
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block();
    let l1_block_1_prime = block_info_from(h.l1.tip());

    // Sync the SharedL1Chain that the verifier's providers read from.
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    // Reset the pipeline: revert safe head and L1 origin to genesis.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = l2_genesis_for(&rollup_cfg, l1_genesis);
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    // Drain the reset origin (genesis has no batch data).
    verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");

    // Signal the new fork's empty block 1' and step.
    verifier.act_l1_head_signal(l1_block_1_prime).await.expect("signal block 1'");
    let derived = verifier.act_l2_pipeline_full().await.expect("step after reorg");

    assert_eq!(derived, 0, "no batch in reorged fork");
    assert_eq!(verifier.l2_safe().block_info.number, 0, "safe head reverted to genesis");
}

/// After a reorg, the batcher resubmits the lost frame in a new L1 block on
/// the canonical fork. The verifier must re-derive the same L2 block from the
/// new inclusion block, recovering the safe head back to 1.
#[tokio::test]
async fn reorg_and_resubmit_rederives_l2_block() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_genesis_hash = h.l1.chain()[0].hash();

    // --- Pre-reorg: derive L2 block 1 from L1 block 1. ---
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_cfg.clone());
    batcher.advance().expect("batcher encode");
    drop(batcher);
    h.l1.mine_block();

    let (mut verifier, chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize");
    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");
    assert_eq!(derived, 1);
    assert_eq!(verifier.l2_safe().block_info.number, 1);

    // --- Reorg: truncate L1 to genesis; mine an empty block 1'. ---
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block(); // block 1' (empty)
    let l1_block_1_prime = block_info_from(h.l1.tip());
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    // Reset pipeline to genesis.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = l2_genesis_for(&rollup_cfg, l1_genesis);
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");

    // Step over the empty block 1' — nothing derived.
    verifier.act_l1_head_signal(l1_block_1_prime).await.expect("signal block 1'");
    let empty = verifier.act_l2_pipeline_full().await.expect("step over block 1'");
    assert_eq!(empty, 0, "block 1' has no batch; nothing derived");
    assert_eq!(verifier.l2_safe().block_info.number, 0);

    // --- Resubmit: encode L2 block 1 again; mine L1 block 2'. ---
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("batcher re-encode on new fork");
    drop(batcher);
    h.l1.mine_block(); // block 2'
    chain.push(h.l1.tip().clone());

    // Derive L2 block 1 from the resubmitted batch in L1 block 2'.
    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 2'");
    let rederived = verifier.act_l2_pipeline_full().await.expect("step block 2'");

    assert_eq!(rederived, 1, "L2 block 1 re-derived from resubmitted batch");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head recovered to 1");
}

/// The canonical chain flip-flops between two competing forks (A and B) three
/// times.  After each switch the pipeline is reset and must re-derive the same
/// L2 block from whichever fork is currently canonical, without accumulating
/// stale channel or frame data from a previous fork.
///
/// This is the analogue of op-e2e's `ReorgFlipFlop` scenario.
#[tokio::test]
async fn reorg_flip_flop() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_genesis_hash = h.l1.chain()[0].hash();
    let l2_block_1 = MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    };

    // Shared reset helpers — computed once, valid across all forks because
    // genesis is immutable.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = l2_genesis_for(&rollup_cfg, l1_genesis);
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    // --- Phase 1: Fork A canonical (genesis → A1 with batch). ---
    h.push_l2_block(l2_block_1.clone());
    let mut batcher = h.create_batcher(batcher_cfg.clone());
    batcher.advance().expect("A1 batcher encode");
    drop(batcher);
    h.l1.mine_block(); // A1

    let (mut verifier, chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize");
    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal A1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step A");
    assert_eq!(derived, 1, "phase 1: L2 block 1 derived from fork A");
    assert_eq!(verifier.l2_safe().block_info.number, 1);

    // --- Phase 2: Fork B canonical (reorg A; mine B1 with the same batch). ---
    h.l1.reorg_to(0).expect("reorg to fork B");
    h.push_l2_block(l2_block_1.clone());
    let mut batcher = h.create_batcher(batcher_cfg.clone());
    batcher.advance().expect("B1 batcher encode");
    drop(batcher);
    h.l1.mine_block(); // B1
    let fork_b1 = block_info_from(h.l1.tip());

    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset to B");
    verifier.act_l2_pipeline_full().await.expect("drain genesis (reset to B)");
    verifier.act_l1_head_signal(fork_b1).await.expect("signal B1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step B");
    assert_eq!(derived, 1, "phase 2: L2 block 1 re-derived from fork B");
    assert_eq!(verifier.l2_safe().block_info.number, 1);

    // --- Phase 3: Fork A' canonical (reorg B; mine A1' — same batch, new fork). ---
    h.l1.reorg_to(0).expect("reorg to fork A'");
    h.push_l2_block(l2_block_1);
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("A1' batcher encode");
    drop(batcher);
    h.l1.mine_block(); // A1'
    let fork_a_prime1 = block_info_from(h.l1.tip());

    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset to A'");
    verifier.act_l2_pipeline_full().await.expect("drain genesis (reset to A')");
    verifier.act_l1_head_signal(fork_a_prime1).await.expect("signal A1'");
    let derived = verifier.act_l2_pipeline_full().await.expect("step A'");
    assert_eq!(derived, 1, "phase 3: L2 block 1 re-derived from fork A'");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}

/// A batch submitted at the last valid L1 block within the sequence window
/// must be derived successfully.
///
/// With `seq_window_size = 4` and `epoch = 0`, the valid inclusion range is
/// L1 blocks 1–3 (strictly: `epoch(0) + window(4) > inclusion_block`).
/// Submitting the batch in block 3 — the final valid slot — must succeed.
///
/// This is the analogue of op-e2e's `BatchInLastPossibleBlocks` scenario.
#[tokio::test]
async fn batch_accepted_at_last_seq_window_block() {
    const SEQ_WINDOW: u64 = 4;

    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = RollupConfig {
        batch_inbox_address: batcher_cfg.inbox_address,
        block_time: 2,
        max_sequencer_drift: 600,
        seq_window_size: SEQ_WINDOW,
        channel_timeout: 300,
        genesis: ChainGenesis {
            system_config: Some(SystemConfig {
                batcher_address: batcher_cfg.batcher_address,
                gas_limit: 30_000_000,
                ..Default::default()
            }),
            ..Default::default()
        },
        hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
        ..Default::default()
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_genesis_hash = h.l1.chain()[0].hash();

    // L2 block 1 references L1 genesis (epoch 0).
    h.push_l2_block(MockL2Block {
        number: 1,
        timestamp: 2,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });

    // Mine 2 empty L1 blocks (no batch yet).
    h.mine_l1_blocks(2); // blocks 1 and 2

    // Submit batch and mine L1 block 3 — the last valid inclusion block for
    // epoch 0 with seq_window_size = 4 (valid iff inclusion_block < 4).
    let mut batcher = h.create_batcher(batcher_cfg);
    batcher.advance().expect("batcher encode");
    drop(batcher);
    h.l1.mine_block(); // block 3

    let (mut verifier, _chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize");

    // Signal blocks 1, 2, 3 and step after each.
    for i in 1..=SEQ_WINDOW - 1 {
        let block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        1,
        "batch in last valid L1 block must be derived"
    );
}

/// Build a `ConfigUpdate` log encoding a batcher-address rotation.
///
/// The log is addressed to `l1_sys_cfg_addr` so that the derivation
/// pipeline's [`IndexedTraversal`] recognises and applies it when reading
/// the containing block's receipts via `update_with_receipts`.
///
/// Log layout (mirrors the on-chain `SystemConfig.sol` event):
/// - `topics[0]` = `CONFIG_UPDATE_TOPIC`
/// - `topics[1]` = `CONFIG_UPDATE_EVENT_VERSION_0` (`B256::ZERO`)
/// - `topics[2]` = `B256::ZERO` (`UpdateType::BatcherAddress = 0`)
/// - `data`      = ABI-encoded `bytes`: pointer(0x20) ++ length(0x20) ++
///   right-aligned 20-byte batcher address
///
/// [`IndexedTraversal`]: base_consensus_derive::IndexedTraversal
fn batcher_update_log(l1_sys_cfg_addr: Address, new_batcher: Address) -> alloy_primitives::Log {
    // 96 bytes: pointer (32) + length (32) + padded address (32).
    let mut data = [0u8; 96];
    data[31] = 0x20; // pointer → offset 32
    data[63] = 0x20; // length  → 32 bytes
    data[76..96].copy_from_slice(new_batcher.as_slice()); // address, right-aligned

    alloy_primitives::Log {
        address: l1_sys_cfg_addr,
        data: LogData::new_unchecked(
            vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, B256::ZERO],
            data.into(),
        ),
    }
}

/// After a batcher-address rotation committed to L1 via a `ConfigUpdate` log,
/// frames from the old batcher address are silently ignored and frames from
/// the new address are derived normally.
///
/// The rotation is delivered as a real `ConfigUpdate` log in an L1 receipt.
/// The [`IndexedTraversal`] stage reads receipts via `receipts_by_hash` when
/// advancing L1 origin, and calls `update_with_receipts` to update its
/// internal [`SystemConfig`].  Subsequent calls to
/// `DataAvailabilityProvider::next` receive the updated batcher address, so
/// the old batcher's frames are filtered out at the frame-retrieval layer.
///
/// Flow:
///   L1 blocks 1-2: batcher A submits → L2 blocks 1-2 derived
///   L1 block 3:    rotation log only  → system config updated, 0 L2 blocks
///   L1 block 4:    batcher A submits  → IGNORED (0 derived)
///   L1 block 5:    batcher B submits  → DERIVED (1 derived)
///
/// This is the acceptance/rejection half of op-e2e's `BatcherKeyRotation`
/// scenario (the reorg-of-rotation reversal is left for a future test once
/// receipt log infrastructure is complete).
///
/// [`IndexedTraversal`]: base_consensus_derive::IndexedTraversal
/// [`SystemConfig`]: base_consensus_genesis::SystemConfig
#[tokio::test]
async fn batcher_key_rotation_accepts_new_batcher() {
    // Use a dedicated L1 system config address so the pipeline's log filter
    // matches our synthetic ConfigUpdate logs.
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_a = BatcherConfig::default(); // 0xBA…BA
    let batcher_b =
        BatcherConfig { batcher_address: Address::repeat_byte(0xBB), ..batcher_a.clone() };

    let rollup_cfg = RollupConfig {
        batch_inbox_address: batcher_a.inbox_address,
        block_time: 2,
        max_sequencer_drift: 600,
        seq_window_size: 3600,
        channel_timeout: 300,
        l1_system_config_address: l1_sys_cfg_addr,
        genesis: ChainGenesis {
            system_config: Some(SystemConfig {
                batcher_address: batcher_a.batcher_address,
                gas_limit: 30_000_000,
                ..Default::default()
            }),
            ..Default::default()
        },
        hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
        ..Default::default()
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_genesis_hash = h.l1.chain()[0].hash();

    // --- L1 blocks 1-2: batcher A submits → L2 blocks 1-2 derived. ---
    for i in 1u64..=2 {
        h.push_l2_block(MockL2Block {
            number: i,
            timestamp: i * 2,
            l1_origin_number: 0,
            l1_origin_hash: l1_genesis_hash,
            ..Default::default()
        });
        let mut batcher = h.create_batcher(batcher_a.clone());
        batcher.advance().expect("batcher A encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // --- L1 block 3: rotation log only, no batch. ---
    // After the traversal processes this block and reads the ConfigUpdate log
    // from its receipts, the pipeline's internal batcher address switches to B.
    h.l1.enqueue_log(batcher_update_log(l1_sys_cfg_addr, batcher_b.batcher_address));
    h.l1.mine_block(); // block 3: rotation receipt, no batcher tx

    let (mut verifier, chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize");

    // Drive derivation through blocks 1-2 (batcher A frames derived).
    for i in 1u64..=2 {
        let block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2, "blocks 1-2 derived with batcher A");

    // Step over the rotation block — no batch, but system config updates to B.
    let rotation_block = block_info_from(h.l1.block_by_number(3).expect("rotation block"));
    verifier.act_l1_head_signal(rotation_block).await.expect("signal rotation block");
    let rotation_derived = verifier.act_l2_pipeline_full().await.expect("step rotation block");
    assert_eq!(rotation_derived, 0, "rotation block contains no batch");

    // --- L1 block 4: batcher A submits for L2 block 3 — must be ignored. ---
    h.push_l2_block(MockL2Block {
        number: 3,
        timestamp: 6,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_a.clone());
    batcher.advance().expect("batcher A encode block 3");
    drop(batcher);
    h.l1.mine_block(); // block 4 — A's frame
    chain.push(h.l1.tip().clone());

    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 4");
    let derived_a = verifier.act_l2_pipeline_full().await.expect("step block 4");
    assert_eq!(derived_a, 0, "batcher A frame must be ignored after key rotation");
    assert_eq!(verifier.l2_safe().block_info.number, 2, "safe head must not advance");

    // --- L1 block 5: batcher B submits for L2 block 3 — must be derived. ---
    h.push_l2_block(MockL2Block {
        number: 3,
        timestamp: 6,
        l1_origin_number: 0,
        l1_origin_hash: l1_genesis_hash,
        ..Default::default()
    });
    let mut batcher = h.create_batcher(batcher_b);
    batcher.advance().expect("batcher B encode block 3");
    drop(batcher);
    h.l1.mine_block(); // block 5 — B's frame
    chain.push(h.l1.tip().clone());

    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 5");
    let derived_b = verifier.act_l2_pipeline_full().await.expect("step block 5");
    assert_eq!(derived_b, 1, "batcher B frame must be derived after key rotation");
    assert_eq!(verifier.l2_safe().block_info.number, 3, "safe head advances to 3");
}
