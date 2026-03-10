#![doc = "Action tests for L2 derivation via the verifier pipeline."]

use alloy_primitives::{Address, B256, Bytes, LogData};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, ChannelDriverConfig, L1MinerConfig,
    PendingTx, SharedL1Chain, block_info_from,
};
use base_consensus_genesis::{
    CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC, ChainGenesis, HardForkConfig, RollupConfig,
    SystemConfig,
};
use base_protocol::DERIVATION_VERSION_0;

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

    // Build L2 block 1 using the L2BlockBuilder, which automatically computes
    // epoch_num=0 and epoch_hash from the L1 genesis block.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build L2 block 1"));

    // Encode the L2 block into a batcher frame and submit to the L1 pending pool.
    let mut batcher = h.create_batcher(source, batcher_cfg);
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

    // Build L2 blocks 1-3 from genesis. With block_time=2 and L1 block_time=12,
    // all three blocks (timestamps 2, 4, 6 s) stay in epoch 0 (genesis, ts=0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);

    // Track block hashes so the verifier's safe-head hash stays consistent
    // with the builder's real sealed headers.
    let mut block_hashes = Vec::new();
    for _ in 1..=L2_BLOCK_COUNT {
        let mut source = ActionL2Source::new();
        source.push(builder.build_next_block().expect("build block"));
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));

        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher advance");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
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

    // Encode L2 block 1 and mine L1 block 1 containing the batcher frame.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build block 1"));

    let mut batcher = h.create_batcher(source, batcher_cfg);
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

    // Batch and mine L1 block 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build block 1"));

    let mut batcher = h.create_batcher(source, batcher_cfg);
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
    let l2_genesis = h.l2_genesis();
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

    // --- Pre-reorg: derive L2 block 1 from L1 block 1. ---
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let block1 = builder.build_next_block().expect("build block 1");

    let mut source = ActionL2Source::new();
    source.push(block1.clone());
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
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
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");

    // Step over the empty block 1' — nothing derived.
    verifier.act_l1_head_signal(l1_block_1_prime).await.expect("signal block 1'");
    let empty = verifier.act_l2_pipeline_full().await.expect("step over block 1'");
    assert_eq!(empty, 0, "block 1' has no batch; nothing derived");
    assert_eq!(verifier.l2_safe().block_info.number, 0);

    // --- Resubmit: re-encode block 1 in L1 block 2'. ---
    // The same block 1 (cloned) re-submitted with the same epoch info will be
    // accepted by the pipeline on the new fork.
    let mut source2 = ActionL2Source::new();
    source2.push(block1);
    let mut batcher = h.create_batcher(source2, batcher_cfg);
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

    // Build L2 block 1 once; we re-use (clone) it across all forks since the
    // epoch info is the same (all forks reference L1 genesis as epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let block1 = h.create_l2_builder(l1_chain).build_next_block().expect("build block 1");

    // Shared reset helpers — computed once, valid across all forks because
    // genesis is immutable.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    // --- Phase 1: Fork A canonical (genesis → A1 with batch). ---
    let mut source = ActionL2Source::new();
    source.push(block1.clone());
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
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
    let mut source = ActionL2Source::new();
    source.push(block1.clone());
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
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
    let mut source = ActionL2Source::new();
    source.push(block1);
    let mut batcher = h.create_batcher(source, batcher_cfg);
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

    // Build L2 block 1 referencing L1 genesis (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build block 1"));

    // Mine 2 empty L1 blocks (no batch yet).
    h.mine_l1_blocks(2); // blocks 1 and 2

    // Submit batch and mine L1 block 3 — the last valid inclusion block for
    // epoch 0 with seq_window_size = 4 (valid iff inclusion_block < 4).
    let mut batcher = h.create_batcher(source, batcher_cfg);
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

    // Build all L2 blocks (1, 2, and 3) upfront from the L1 genesis state.
    // With block_time=2 and L1 block_time=12, all three (timestamps 2,4,6s)
    // stay in epoch 0 (genesis ts=0), so the builder picks the same L1 origin.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let block1 = builder.build_next_block().expect("build block 1");
    let hash1 = builder.head().block_info.hash;
    let block2 = builder.build_next_block().expect("build block 2");
    let hash2 = builder.head().block_info.hash;
    let block3 = builder.build_next_block().expect("build block 3");
    let hash3 = builder.head().block_info.hash;

    // --- L1 blocks 1-2: batcher A submits → L2 blocks 1-2 derived. ---
    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_a.clone());
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
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.register_block_hash(3, hash3);
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
    let mut source_a = ActionL2Source::new();
    source_a.push(block3.clone());
    let mut batcher = h.create_batcher(source_a, batcher_a.clone());
    batcher.advance().expect("batcher A encode block 3");
    drop(batcher);
    h.l1.mine_block(); // block 4 — A's frame
    chain.push(h.l1.tip().clone());

    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 4");
    let derived_a = verifier.act_l2_pipeline_full().await.expect("step block 4");
    assert_eq!(derived_a, 0, "batcher A frame must be ignored after key rotation");
    assert_eq!(verifier.l2_safe().block_info.number, 2, "safe head must not advance");

    // --- L1 block 5: batcher B submits for L2 block 3 — must be derived. ---
    let mut source_b = ActionL2Source::new();
    source_b.push(block3);
    let mut batcher = h.create_batcher(source_b, batcher_b);
    batcher.advance().expect("batcher B encode block 3");
    drop(batcher);
    h.l1.mine_block(); // block 5 — B's frame
    chain.push(h.l1.tip().clone());

    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 5");
    let derived_b = verifier.act_l2_pipeline_full().await.expect("step block 5");
    assert_eq!(derived_b, 1, "batcher B frame must be derived after key rotation");
    assert_eq!(verifier.l2_safe().block_info.number, 3, "safe head advances to 3");
}

/// Derive 6 L2 blocks all belonging to the same L1 epoch (genesis).
///
/// With `block_time=2` and L1 `block_time=12`, L2 blocks 1-6 (timestamps
/// 2,4,6,8,10,12) all reference epoch 0 because the first L1 block after
/// genesis has not been mined yet when the blocks are built. Each L2 block
/// is batched into a separate L1 inclusion block.
#[tokio::test]
async fn multi_l2_per_l1_epoch() {
    const L2_COUNT: u64 = 6;
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);

    let (mut verifier, chain) = h.create_verifier();

    for i in 1..=L2_COUNT {
        let block = builder.build_next_block().expect("build L2 block");
        let hash = builder.head().block_info.hash;
        verifier.register_block_hash(i, hash);

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher advance");
        drop(batcher);

        h.mine_and_push(&chain);
    }

    verifier.initialize().await.expect("initialize");

    for i in 1..=L2_COUNT {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("step");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        L2_COUNT,
        "safe head should be at L2 block {L2_COUNT}"
    );
    assert_eq!(verifier.l2_safe().l1_origin.number, 0, "all blocks in epoch 0");
}

/// A batch submitted past the sequence window is rejected and the pipeline
/// generates deposit-only (default) blocks to fill the epoch instead.
///
/// With `seq_window_size = 3` and epoch 0, valid inclusion blocks are those
/// with `inclusion_block < epoch + seq_window = 0 + 3 = 3`, i.e. blocks 1
/// and 2. A batch included in block 3 is past the window and is dropped.
///
/// When the sequence window closes without a valid batch for an epoch, the
/// pipeline generates default (deposit-only) blocks for all L2 slots in
/// that epoch. This means the safe head still advances — but only with
/// deposit-only blocks, not with the user-submitted batch content.
///
/// Contrast with [`batch_accepted_at_last_seq_window_block`] where the
/// batch is submitted inside the window and is accepted.
#[tokio::test]
async fn batch_past_sequence_window_rejected() {
    const SEQ_WINDOW: u64 = 3;
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

    // Build L2 block 1 (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build block 1"));

    // Mine 2 empty L1 blocks (seq_window=3, so valid inclusion is blocks 1 and 2 only).
    // Batch is valid if inclusion_block < epoch + seq_window = 0 + 3 = 3.
    // So blocks 1 and 2 are valid, block 3 is NOT.
    h.mine_l1_blocks(2); // mine blocks 1 and 2 (no batch yet)

    // Submit batch in block 3 — past the window.
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode");
    drop(batcher);
    h.l1.mine_block(); // block 3

    let (mut verifier, _chain) = h.create_verifier();
    verifier.initialize().await.expect("initialize");

    let mut total_derived = 0;
    for i in 1..=SEQ_WINDOW {
        let block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(block).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    // The pipeline generates deposit-only blocks to fill the epoch when the
    // sequence window expires without a valid batch. The safe head advances
    // past genesis because default blocks are still derived.
    assert!(
        verifier.l2_safe().block_info.number > 0,
        "pipeline should generate deposit-only blocks when sequence window expires"
    );
    assert!(
        total_derived > 0,
        "pipeline should derive deposit-only (default) blocks for the expired epoch"
    );
    // All derived blocks are in epoch 0 since no L1 epoch boundary was crossed.
    assert_eq!(
        verifier.l2_safe().l1_origin.number,
        0,
        "all deposit-only blocks should reference epoch 0"
    );
}

/// Build 12 L2 blocks spanning two epoch boundaries (epoch 0 → 1 → 2).
///
/// With `block_time=2` and L1 `block_time=12`:
/// - L2 blocks 1-5 (timestamps 2-10) reference epoch 0 (L1 genesis at ts=0)
/// - L2 blocks 6-11 (timestamps 12-22) reference epoch 1 (L1 block 1 at ts=12)
/// - L2 block 12 (timestamp 24) references epoch 2 (L1 block 2 at ts=24)
///
/// Each L2 block is batched into its own L1 inclusion block. The test verifies
/// that all 12 blocks are derived and the final safe head reaches block 12.
#[tokio::test]
async fn multi_epoch_sequence() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 blocks 1 and 2 so the builder can advance epochs.
    h.mine_l1_blocks(2);

    // Build 12 L2 blocks from genesis using a shared L1 chain that includes
    // the two mined L1 blocks for epoch advancement.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);

    let mut blocks = Vec::new();
    let mut block_hashes = Vec::new();
    for _ in 0..12 {
        let block = builder.build_next_block().expect("build L2 block");
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        blocks.push(block);
    }

    // Verify the builder's epoch assignment at the boundary.
    // L2 block 6 (index 5) should be the first block in epoch 1.
    assert_eq!(builder.head().l1_origin.number, 2, "L2 block 12 should reference epoch 2");

    // Create verifier and register all block hashes.
    let (mut verifier, chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }

    // Batch each L2 block into a separate L1 inclusion block.
    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("batcher advance");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    verifier.initialize().await.expect("initialize");

    // Drive derivation through all L1 blocks: blocks 1-2 are epoch-providing
    // (no batches), blocks 3-14 each contain one batch.
    let mut total_derived = 0;
    for i in 1..=(2 + 12) {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        total_derived += verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(total_derived, 12, "all 12 L2 blocks should be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 12, "safe head should reach L2 block 12");
}

/// Build 3 L2 blocks, encode all 3 into a single batcher submission (one
/// channel), mine one L1 block, and verify that all 3 are derived from that
/// single L1 block.
///
/// This tests that the pipeline correctly handles multiple batches within a
/// single channel frame delivered in one L1 block.
#[tokio::test]
async fn same_epoch_multi_batch_one_l1_block() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);

    let mut source = ActionL2Source::new();
    let mut block_hashes = Vec::new();
    for _ in 1..=3u64 {
        let block = builder.build_next_block().expect("build");
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        source.push(block);
    }

    // Encode all 3 blocks into one batcher submission (single channel).
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("batcher encode 3 blocks");
    drop(batcher);

    // Mine ONE L1 block containing all 3 batches.
    h.l1.mine_block();

    // Create verifier after mining so the snapshot includes the inclusion block.
    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");

    assert_eq!(derived, 3, "all 3 L2 blocks should be derived from one L1 block");
    assert_eq!(verifier.l2_safe().block_info.number, 3);
}

/// Derive 5 L2 blocks, reorg L1 all the way back to genesis, resubmit all 5
/// batches on the new fork, and verify the safe head recovers to 5.
///
/// This is a deeper reorg than [`reorg_reverts_derived_safe_head`] which only
/// tests a single-block reorg. Here the verifier must correctly reset its
/// internal state (channels, batch queue, safe head) after a deep reorg that
/// removes 5 L1 inclusion blocks.
#[tokio::test]
async fn deep_reorg_multi_block() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);

    // Build 5 L2 blocks.
    let mut blocks = Vec::new();
    let mut block_hashes = Vec::new();
    for _ in 0..5 {
        let block = builder.build_next_block().expect("build");
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
        blocks.push(block);
    }

    // Submit each block's batch individually and mine an L1 block for each.
    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // Create verifier with all 5 L1 inclusion blocks visible.
    let (mut verifier, chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    for i in 1..=5u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 5, "pre-reorg: 5 L2 blocks derived");

    // Reorg all the way back to genesis.
    h.l1.reorg_to(0).expect("reorg to genesis");
    chain.truncate_to(0);

    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis"));
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();
    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    verifier.act_l2_pipeline_full().await.expect("drain after reset");

    assert_eq!(verifier.l2_safe().block_info.number, 0, "safe head reverted to genesis");

    // Re-submit all 5 batches on the new fork.
    for block in &blocks {
        let mut source = ActionL2Source::new();
        source.push(block.clone());
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("re-encode");
        drop(batcher);
        h.mine_and_push(&chain);
    }

    // Drive derivation on the new fork.
    for i in 1..=5u64 {
        let new_l1_block = block_info_from(h.l1.block_by_number(i).expect("block"));
        verifier.act_l1_head_signal(new_l1_block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(verifier.l2_safe().block_info.number, 5, "post-reorg: 5 L2 blocks recovered");
}

/// Garbage frame data (valid derivation version prefix but corrupt frame
/// bytes) must be silently ignored by the pipeline. A valid batch submitted
/// in a subsequent L1 block must still derive correctly.
///
/// This verifies the `ChannelBank`'s robustness: malformed frames are
/// dropped without crashing the pipeline or poisoning subsequent channels.
#[tokio::test]
async fn garbage_frame_data_ignored() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    let block = builder.build_next_block().expect("build L2 block 1");
    let hash1 = builder.head().block_info.hash;
    source.push(block);

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.initialize().await.expect("initialize");

    // Submit a garbage batcher tx: valid derivation version prefix + random bytes.
    // The ChannelBank should reject the malformed frame and not crash.
    let garbage = {
        let mut v = vec![DERIVATION_VERSION_0];
        v.extend_from_slice(&[0xFF, 0xAB, 0x12, 0x34, 0x56, 0x78]);
        Bytes::from(v)
    };
    h.l1.submit_tx(PendingTx {
        from: batcher_cfg.batcher_address,
        to: batcher_cfg.inbox_address,
        input: garbage,
    });
    h.mine_and_push(&chain);

    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal garbage block");
    let derived = verifier.act_l2_pipeline_full().await.expect("step over garbage");
    assert_eq!(derived, 0, "garbage frame must be silently ignored");
    assert_eq!(verifier.l2_safe().block_info.number, 0);

    // Now submit the real batch.
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode");
    drop(batcher);
    h.mine_and_push(&chain);

    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal real block");
    let derived = verifier.act_l2_pipeline_full().await.expect("step real block");
    assert_eq!(derived, 1, "real frame after garbage must still be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}

/// A channel whose compressed data exceeds `max_frame_size` is split across
/// multiple frames. All frames are submitted in the same L1 block (as separate
/// transactions) and the `ChannelBank` reassembles them into the original
/// channel data, deriving the L2 block.
///
/// This exercises the `ChannelDriver` multi-frame output path and verifies
/// that [`Batcher::encode_frames`] / [`Batcher::submit_frames`] correctly
/// produce multiple frame transactions that the derivation pipeline reassembles.
///
/// NOTE: The `IndexedTraversal` mode clears the `ChannelBank` on each
/// `ProvideBlock` signal, so multi-L1-block channels are not supported in this
/// test harness. All frames must land in the same L1 block.
#[tokio::test]
async fn multi_frame_channel_reassembled() {
    let batcher_cfg = BatcherConfig {
        // A very small frame size forces the channel to spill across
        // multiple frames even for a single L2 block's batch data.
        driver: ChannelDriverConfig { max_frame_size: 80 },
        ..BatcherConfig::default()
    };
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_builder(l1_chain);
    let mut source = ActionL2Source::new();
    let block = builder.build_next_block().expect("build L2 block 1");
    let hash1 = builder.head().block_info.hash;
    source.push(block);

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);

    // Encode the L2 block. With max_frame_size=80, the compressed channel data
    // should spill across multiple frames.
    let mut batcher = h.create_batcher(source, batcher_cfg);
    let frames = batcher.encode_frames().expect("encode");
    assert!(
        frames.len() >= 2,
        "expected channel to split into 2+ frames with max_frame_size=80, got {}",
        frames.len()
    );

    // Verify frame structure: sequential numbers, same channel ID, only last frame has is_last.
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(frame.number, i as u16, "frame {i} should have number {i}");
        assert_eq!(frame.id, frames[0].id, "all frames should share the same channel ID");
        if i < frames.len() - 1 {
            assert!(!frame.is_last, "intermediate frame {i} must not be marked as last");
        } else {
            assert!(frame.is_last, "final frame must be marked as last");
        }
    }

    // Submit ALL frames to the same L1 block (each as a separate tx).
    batcher.submit_frames(&frames);
    drop(batcher);
    h.mine_and_push(&chain);

    verifier.initialize().await.expect("initialize");
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step block 1");
    assert_eq!(derived, 1, "multi-frame channel should be reassembled and derived");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}
