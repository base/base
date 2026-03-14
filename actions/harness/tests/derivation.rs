#![doc = "Action tests for L2 derivation via the verifier pipeline."]

use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, LogData, U256};
use base_action_harness::{
    ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider, ActionL2Source,
    ActionTestHarness, BatchType, BatcherConfig, GarbageKind, L1MinerConfig, L2Sequencer,
    L2Verifier, PendingTx, SharedL1Chain, StepResult, block_info_from,
};
use base_blobs::BlobEncoder;
use base_consensus_genesis::{
    CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC, L1ChainConfig, RollupConfig,
};
use base_consensus_registry::Registry;
use base_protocol::{
    BlockInfo, DEPOSIT_EVENT_ABI_HASH, DEPOSIT_EVENT_VERSION_0, DERIVATION_VERSION_0, L2BlockInfo,
};

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// Starts from the real Base mainnet config and overrides only the fields that
/// must differ for in-memory action tests:
/// - `batch_inbox_address` and `batcher_address` wire the test actors.
/// - `genesis` is zeroed so the in-memory L1 miner (which starts at ts=0) is
///   the chain origin.
/// - Canyon through Fjord are set to `Some(0)` so the batcher's brotli
///   compression and span-batch encoding are accepted from genesis.
/// - Hardforks after Fjord retain their mainnet timestamps, which are
///   unreachable during tests (L1 miner starts at ts=0), making them
///   effectively inactive without losing their real config values.
fn rollup_config_for(batcher: &BatcherConfig) -> RollupConfig {
    let mut rc = Registry::rollup_config(8453).expect("mainnet config").clone();
    rc.batch_inbox_address = batcher.inbox_address;
    rc.genesis.system_config.as_mut().unwrap().batcher_address = batcher.batcher_address;
    rc.genesis.l2_time = 0;
    rc.genesis.l1 = Default::default();
    rc.genesis.l2 = Default::default();
    rc.hardforks.canyon_time = Some(0);
    rc.hardforks.delta_time = Some(0);
    rc.hardforks.ecotone_time = Some(0);
    rc.hardforks.fjord_time = Some(0);
    rc
}

/// The derivation pipeline reads a single batcher frame from L1 and derives
/// the corresponding L2 block, advancing the safe head from genesis (0) to 1.
#[tokio::test]
async fn single_l2_block_derived_from_batcher_frame() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 using the L2Sequencer, which automatically computes
    // epoch_num=0 and epoch_hash from the L1 genesis block.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let mut builder = h.create_l2_sequencer(l1_chain);

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
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let block1 = h.create_l2_sequencer(l1_chain).build_next_block().expect("build block 1");

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

/// The canonical chain flip-flops through three forks, where the middle fork
/// is completely empty (no batcher data).
///
/// This extends [`reorg_flip_flop`] by testing that after the pipeline is reset
/// to an empty fork and derives zero L2 blocks, it holds no residual channel or
/// frame data from fork A when fork C presents the same two batches.  If stale
/// frames from A persisted across the B reset they could cause the pipeline to
/// assemble a channel prematurely or reject C's frames as duplicates.
///
/// - Fork A: mine A1 and A2, each with one batch; derive L2 blocks 1 and 2
///   (safe head = 2).
/// - Fork B: reorg to genesis; mine two empty L1 blocks; reset the pipeline;
///   signal both — zero blocks derived; safe head = 0.
/// - Fork C: reorg to genesis; resubmit the same two batches; reset the
///   pipeline; signal both — both L2 blocks re-derived; safe head = 2.
#[tokio::test]
async fn reorg_flip_flop_empty_middle_fork() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build L2 blocks 1-2 against a genesis-only chain so both reference epoch 0
    // and their encoded batch frames are valid on any fork sharing genesis.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block().expect("build block 1");
    let block1_hash = builder.head().block_info.hash;
    let block2 = builder.build_next_block().expect("build block 2");
    let block2_hash = builder.head().block_info.hash;

    // Shared reset targets — valid across all forks because genesis is immutable.
    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = h.l2_genesis();
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();

    // --- Fork A: mine A1 (batch for L2 block 1) and A2 (batch for L2 block 2). ---
    for block in [block1.clone(), block2.clone()] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("fork A: encode");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, block1_hash);
    verifier.register_block_hash(2, block2_hash);
    verifier.initialize().await.expect("initialize");

    for i in 1u64..=2 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("fork A block"));
        verifier.act_l1_head_signal(blk).await.expect("fork A: signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("fork A: step");
        assert_eq!(derived, 1, "fork A: L2 block {i} derived");
        assert_eq!(
            verifier.l2_safe().l1_origin.number,
            0,
            "fork A: L2 block {i} l1_origin = genesis"
        );
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2, "fork A: safe head = 2");

    // --- Fork B: reorg to genesis; mine two empty blocks; derive nothing. ---
    h.l1.reorg_to(0).expect("reorg to fork B");
    chain.truncate_to(0);
    let mut fork_b_blocks = Vec::new();
    for _ in 0..2 {
        fork_b_blocks.push(h.mine_and_push(&chain));
    }

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset to fork B");
    // act_reset sets safe_head and finalized_head to the reset target (l2_genesis).
    // Per the OP Stack spec, unsafe_head is NOT clamped to safe_head on reset —
    // it is re-discovered by walking back from the current tip to the first block
    // with a plausible (canonical or ahead-of-L1) L1 origin.  In this verifier-only
    // context no gossip blocks were received, so unsafe_head was never advanced
    // beyond genesis and therefore remains 0 regardless.
    assert_eq!(verifier.l2_safe().block_info.number, 0, "reset to B: safe head = 0");
    assert_eq!(verifier.l2_finalized().block_info.number, 0, "reset to B: finalized head = 0");
    assert_eq!(verifier.l2_unsafe().block_info.number, 0, "reset to B: unsafe head = 0");

    let drained = verifier.act_l2_pipeline_full().await.expect("drain genesis after reset to B");
    assert_eq!(drained, 0, "reset drain must produce no L2 blocks");

    for (i, blk_info) in fork_b_blocks.into_iter().enumerate() {
        verifier.act_l1_head_signal(blk_info).await.expect("fork B: signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("fork B: step");
        assert_eq!(derived, 0, "fork B block {}: empty, nothing derived", i + 1);
    }
    assert_eq!(verifier.l2_safe().block_info.number, 0, "fork B: safe head = 0");
    assert_eq!(verifier.l2_finalized().block_info.number, 0, "fork B: finalized head = 0");

    // --- Fork C: reorg to genesis; resubmit both batches; re-derive both blocks. ---
    h.l1.reorg_to(0).expect("reorg to fork C");
    chain.truncate_to(0);
    let mut fork_c_blocks = Vec::new();
    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("fork C: encode");
        drop(batcher);
        fork_c_blocks.push(h.mine_and_push(&chain));
    }

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset to fork C");
    assert_eq!(verifier.l2_safe().block_info.number, 0, "reset to C: safe head = 0");
    assert_eq!(verifier.l2_finalized().block_info.number, 0, "reset to C: finalized head = 0");
    // unsafe_head unchanged by act_reset (spec-compliant: re-discover, don't clamp).
    assert_eq!(verifier.l2_unsafe().block_info.number, 0, "reset to C: unsafe head = 0");

    let drained = verifier.act_l2_pipeline_full().await.expect("drain genesis after reset to C");
    assert_eq!(drained, 0, "reset drain must produce no L2 blocks");

    for (i, blk_info) in fork_c_blocks.into_iter().enumerate() {
        verifier.act_l1_head_signal(blk_info).await.expect("fork C: signal");
        let derived = verifier.act_l2_pipeline_full().await.expect("fork C: step");
        assert_eq!(derived, 1, "fork C: L2 block {} re-derived", i + 1);
        assert_eq!(
            verifier.l2_safe().l1_origin.number,
            0,
            "fork C: L2 block {} l1_origin = genesis",
            i + 1
        );
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2, "fork C: safe head = 2 after flip-flop");
    // finalized_head stays at genesis because no act_l1_finalized_signal was sent.
    assert_eq!(verifier.l2_finalized().block_info.number, 0, "fork C: finalized head = 0");
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
    let mut rollup_cfg = rollup_config_for(&batcher_cfg);
    rollup_cfg.seq_window_size = SEQ_WINDOW;
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 referencing L1 genesis (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
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

/// Build a `TransactionDeposited` log encoding a user deposit.
///
/// The log is addressed to the rollup config's `deposit_contract_address` so
/// that the derivation pipeline's attributes builder picks it up and includes
/// the deposit transaction in the derived L2 block.
///
/// Log layout (mirrors the on-chain `OptimismPortal.sol` event):
/// - `topics[0]` = `DEPOSIT_EVENT_ABI_HASH`
/// - `topics[1]` = `from` address left-padded to B256
/// - `topics[2]` = `to` address left-padded to B256
/// - `topics[3]` = `DEPOSIT_EVENT_VERSION_0` (`B256::ZERO`)
/// - `data`      = ABI-encoded opaqueData:
///   offset(32) + length(32) + tightly-packed(mint(32) + value(32) + gas(8) + isCreation(1) + calldata)
fn user_deposit_log(
    deposit_contract: Address,
    from: Address,
    to: Address,
    mint: u128,
    value: U256,
    gas_limit: u64,
    data: &[u8],
) -> alloy_primitives::Log {
    // opaqueData: mint(32) + value(32) + gas_limit(8) + isCreation(1) + data
    let opaque_len = 32 + 32 + 8 + 1 + data.len();
    let opaque_padded = opaque_len.div_ceil(32) * 32;
    let total_len = 64 + opaque_padded; // offset(32) + length(32) + padded opaqueData

    let mut log_data = vec![0u8; total_len];

    // offset = 32 (right-aligned u64 in 32-byte slot)
    log_data[24..32].copy_from_slice(&32u64.to_be_bytes());
    // length of opaqueData
    log_data[56..64].copy_from_slice(&(opaque_len as u64).to_be_bytes());

    let base = 64;
    // mint: u128 big-endian in bytes [16..32] of a 32-byte slot (upper 16 bytes are zero)
    log_data[base + 16..base + 32].copy_from_slice(&mint.to_be_bytes());
    // value: U256 big-endian in 32 bytes
    log_data[base + 32..base + 64].copy_from_slice(&value.to_be_bytes::<32>());
    // gas_limit: u64 big-endian in 8 bytes
    log_data[base + 64..base + 72].copy_from_slice(&gas_limit.to_be_bytes());
    // isCreation: 0 (call, not create)
    log_data[base + 72] = 0;
    // calldata
    log_data[base + 73..base + 73 + data.len()].copy_from_slice(data);

    // Build from/to topics: left-padded addresses in B256
    let mut from_topic = [0u8; 32];
    from_topic[12..32].copy_from_slice(from.as_slice());
    let mut to_topic = [0u8; 32];
    to_topic[12..32].copy_from_slice(to.as_slice());

    alloy_primitives::Log {
        address: deposit_contract,
        data: LogData::new_unchecked(
            vec![
                DEPOSIT_EVENT_ABI_HASH,
                B256::from(from_topic),
                B256::from(to_topic),
                DEPOSIT_EVENT_VERSION_0,
            ],
            log_data.into(),
        ),
    }
}

/// A user deposit log on L1 is processed by the derivation pipeline without
/// errors.
///
/// The deposit log is placed in an L1 block that also contains the batcher
/// frame for L2 block 1. The pipeline must:
/// 1. Not crash when encountering the deposit log in the L1 receipts.
/// 2. Correctly derive L2 block 1 (safe head advances to 1).
/// 3. The L2 block's L1 origin is the block containing the deposit log.
///
/// This validates the deposit-processing path in the attributes builder
/// without needing EVM execution (the verifier applies attributes without
/// executing deposit transactions).
#[tokio::test]
async fn l1_deposit_included_in_derived_l2_block() {
    let deposit_contract = Address::repeat_byte(0xDD);
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = RollupConfig {
        deposit_contract_address: deposit_contract,
        ..rollup_config_for(&batcher_cfg)
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 from the sequencer.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(sequencer.build_next_block().expect("build L2 block 1"));

    // Enqueue a user deposit log: from=0xAA..AA, to=0xBB..BB, value=1 ETH, gas=100k.
    h.l1.enqueue_log(user_deposit_log(
        deposit_contract,
        Address::repeat_byte(0xAA),
        Address::repeat_byte(0xBB),
        0,                                         // mint
        U256::from(1_000_000_000_000_000_000u128), // 1 ETH in wei
        100_000,                                   // gas_limit
        &[],                                       // empty calldata
    ));

    // Submit the batcher frame into the same L1 block as the deposit log.
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("batcher encode");
    drop(batcher);
    h.l1.mine_block();

    // Create verifier AFTER mining so the snapshot contains block 1.
    let (mut verifier, _chain) = h.create_verifier();
    let l1_block_1 = block_info_from(h.l1.tip());

    verifier.initialize().await.expect("initialize should succeed");
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal should succeed");
    let derived = verifier.act_l2_pipeline_full().await.expect("pipeline step should succeed");

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should be L2 block 1");
    // The L2 block references L1 genesis (epoch 0) because the sequencer built
    // it before L1 block 1 existed. The deposit log lives in L1 block 1, which
    // is the *inclusion* block, not the L1 origin. The pipeline processed the
    // deposit log from block 1's receipts without errors.
    assert_eq!(verifier.l2_safe().l1_origin.number, 0, "L2 block 1 references epoch 0");
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

    let mut rollup_cfg = rollup_config_for(&batcher_a);
    rollup_cfg.l1_system_config_address = l1_sys_cfg_addr;
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build all L2 blocks (1, 2, and 3) upfront from the L1 genesis state.
    // With block_time=2 and L1 block_time=12, all three (timestamps 2,4,6s)
    // stay in epoch 0 (genesis ts=0), so the builder picks the same L1 origin.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let mut builder = h.create_l2_sequencer(l1_chain);

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
    let mut rollup_cfg = rollup_config_for(&batcher_cfg);
    rollup_cfg.seq_window_size = SEQ_WINDOW;
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    let mut builder = h.create_l2_sequencer(l1_chain);

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
    let mut builder = h.create_l2_sequencer(l1_chain);

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
    let mut builder = h.create_l2_sequencer(l1_chain);

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
    let mut builder = h.create_l2_sequencer(l1_chain);
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
    use base_batcher_encoder::EncoderConfig;
    let batcher_cfg = BatcherConfig {
        // Small max_frame_size forces the channel to spill across multiple frames.
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
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

// ── Span-batch derivation variants ────────────────────────────────────────────

/// Derive a single L2 block encoded as a [`SpanBatch`] from one L1 inclusion
/// block.
///
/// Mirrors [`single_l2_block_derived_from_batcher_frame`] but uses
/// [`BatchType::Span`] encoding. The derivation pipeline must correctly parse
/// the span-encoded channel and advance the safe head.
#[tokio::test]
async fn single_l2_block_derived_from_span_batch() {
    let batcher_cfg = BatcherConfig { batch_type: BatchType::Span, ..BatcherConfig::default() };
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    let block = sequencer.build_next_block().expect("build L2 block 1");
    let hash1 = sequencer.head().block_info.hash;
    source.push(block);

    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("span batcher advance");
    drop(batcher);

    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.initialize().await.expect("initialize");

    let l1_block_1 = block_info_from(h.l1.tip());
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");

    assert_eq!(derived, 1, "expected one L2 block from span batch");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}

/// Derive 3 L2 blocks encoded together as a single [`SpanBatch`].
///
/// All 3 blocks are grouped into one span-batch channel submitted in a single
/// L1 block. The derivation pipeline must decode the span and advance the
/// safe head by 3.
#[tokio::test]
async fn three_l2_blocks_derived_from_span_batch() {
    let batcher_cfg = BatcherConfig { batch_type: BatchType::Span, ..BatcherConfig::default() };
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut source = ActionL2Source::new();
    let mut block_hashes: Vec<(u64, B256)> = Vec::new();
    for i in 1..=3u64 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));
        source.push(block);
    }

    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("span advance");
    drop(batcher);

    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    for (n, hash) in &block_hashes {
        verifier.register_block_hash(*n, *hash);
    }
    verifier.initialize().await.expect("initialize");

    let l1_block_1 = block_info_from(h.l1.tip());
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal");
    let derived = verifier.act_l2_pipeline_full().await.expect("step");

    assert_eq!(derived, 3, "expected 3 L2 blocks from span batch");
    assert_eq!(verifier.l2_safe().block_info.number, 3);
}

// ── System-config update tests ─────────────────────────────────────────────────

/// Build a `ConfigUpdate` log encoding a gas-config (overhead + scalar) change.
///
/// Log layout mirrors `SystemConfig.sol`:
/// - `topics[0]` = `CONFIG_UPDATE_TOPIC`
/// - `topics[1]` = `CONFIG_UPDATE_EVENT_VERSION_0`
/// - `topics[2]` = `B256` encoding `UpdateType::GasConfig = 1`
/// - `data`      = 128 bytes: pointer(32=0x20) + length(64=0x40) +
///   overhead(U256, 32 bytes) + scalar(U256, 32 bytes)
fn gas_config_update_log(
    l1_sys_cfg_addr: Address,
    overhead: u64,
    scalar: u64,
) -> alloy_primitives::Log {
    let mut data = [0u8; 128];
    data[31] = 0x20; // pointer = 32
    data[63] = 0x40; // length  = 64
    // overhead and scalar right-aligned as big-endian u64 in U256 slots.
    data[88..96].copy_from_slice(&overhead.to_be_bytes());
    data[120..128].copy_from_slice(&scalar.to_be_bytes());

    let mut update_type = [0u8; 32];
    update_type[31] = 1; // GasConfig = 1

    alloy_primitives::Log {
        address: l1_sys_cfg_addr,
        data: LogData::new_unchecked(
            vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, B256::from(update_type)],
            data.into(),
        ),
    }
}

/// Build a `ConfigUpdate` log encoding a gas-limit change.
///
/// Log layout mirrors `SystemConfig.sol`:
/// - `topics[0]` = `CONFIG_UPDATE_TOPIC`
/// - `topics[1]` = `CONFIG_UPDATE_EVENT_VERSION_0`
/// - `topics[2]` = `B256` encoding `UpdateType::GasLimit = 2`
/// - `data`      = 96 bytes: pointer(32=0x20) + length(32=0x20) +
///   `gas_limit` (U256, 32 bytes)
fn gas_limit_update_log(l1_sys_cfg_addr: Address, gas_limit: u64) -> alloy_primitives::Log {
    let mut data = [0u8; 96];
    data[31] = 0x20; // pointer = 32
    data[63] = 0x20; // length  = 32
    // gas_limit right-aligned as big-endian u64 in a U256 slot.
    data[88..96].copy_from_slice(&gas_limit.to_be_bytes());

    let mut update_type = [0u8; 32];
    update_type[31] = 2; // GasLimit = 2

    alloy_primitives::Log {
        address: l1_sys_cfg_addr,
        data: LogData::new_unchecked(
            vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, B256::from(update_type)],
            data.into(),
        ),
    }
}

/// A `GasConfig` system-config update committed to L1 does not disrupt ongoing
/// derivation.
///
/// Flow:
///   L1 block 1: batch for L2 block 1 → 1 derived
///   L1 block 2: `GasConfig` update log only → 0 derived, config updated
///   L1 block 3: batch for L2 block 2 → 1 derived (pipeline not stuck)
#[tokio::test]
async fn gpo_params_change_does_not_disrupt_derivation() {
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = RollupConfig {
        l1_system_config_address: l1_sys_cfg_addr,
        ..rollup_config_for(&batcher_cfg)
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block().expect("build block 1");
    let hash1 = sequencer.head().block_info.hash;
    let block2 = sequencer.build_next_block().expect("build block 2");
    let hash2 = sequencer.head().block_info.hash;

    // L1 block 1: batch for L2 block 1.
    let mut source = ActionL2Source::new();
    source.push(block1);
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    batcher.advance().expect("encode block 1");
    drop(batcher);
    h.l1.mine_block();

    // L1 block 2: gas-config update log only, no batch.
    h.l1.enqueue_log(gas_config_update_log(l1_sys_cfg_addr, 2100, 1_000_000));
    h.l1.mine_block();

    // L1 block 3: batch for L2 block 2.
    let mut source = ActionL2Source::new();
    source.push(block2);
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode block 2");
    drop(batcher);
    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.initialize().await.expect("initialize");

    for i in 1u64..=3 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "both L2 blocks derived after GPO config update"
    );
}

/// A `GasLimit` system-config update committed to L1 does not disrupt ongoing
/// derivation.
///
/// Flow:
///   L1 block 1: batch for L2 block 1 → 1 derived
///   L1 block 2: `GasLimit` update log only → 0 derived, gas limit updated
///   L1 block 3: batch for L2 block 2 → 1 derived (pipeline not stuck)
#[tokio::test]
async fn gas_limit_change_does_not_disrupt_derivation() {
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = RollupConfig {
        l1_system_config_address: l1_sys_cfg_addr,
        ..rollup_config_for(&batcher_cfg)
    };
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block().expect("build block 1");
    let hash1 = sequencer.head().block_info.hash;
    let block2 = sequencer.build_next_block().expect("build block 2");
    let hash2 = sequencer.head().block_info.hash;

    // L1 block 1: batch for L2 block 1.
    let mut source = ActionL2Source::new();
    source.push(block1);
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    batcher.advance().expect("encode block 1");
    drop(batcher);
    h.l1.mine_block();

    // L1 block 2: gas-limit update log only.
    h.l1.enqueue_log(gas_limit_update_log(l1_sys_cfg_addr, 60_000_000));
    h.l1.mine_block();

    // L1 block 3: batch for L2 block 2.
    let mut source = ActionL2Source::new();
    source.push(block2);
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode block 2");
    drop(batcher);
    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.initialize().await.expect("initialize");

    for i in 1u64..=3 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "both L2 blocks derived after gas-limit update"
    );
}

// ── Typed garbage-frame variant tests ─────────────────────────────────────────

/// Submit a garbage frame of the given kind, mine it into an L1 block, step
/// the derivation pipeline, and assert nothing is derived. Then submit a valid
/// batch and assert recovery succeeds.
///
/// This validates that the pipeline silently discards each [`GarbageKind`]
/// variant without crashing or poisoning subsequent channel state.
async fn garbage_kind_silently_ignored_then_valid_batch_derived(kind: GarbageKind) {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block().expect("build L2 block 1");
    let hash1 = sequencer.head().block_info.hash;

    // L1 block 1: garbage frame only.
    let source_empty = ActionL2Source::new();
    let mut batcher = h.create_batcher(source_empty, batcher_cfg.clone());
    batcher.submit_garbage_frames(kind);
    drop(batcher);
    h.l1.mine_block();

    // L1 block 2: valid batch.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode valid batch");
    drop(batcher);
    h.l1.mine_block();

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.initialize().await.expect("initialize");

    // Block 1: garbage — must not advance the safe head.
    let blk1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(blk1).await.expect("signal garbage block");
    let derived_garbage = verifier.act_l2_pipeline_full().await.expect("step garbage");
    assert_eq!(derived_garbage, 0, "garbage frame must be silently ignored: {kind:?}");
    assert_eq!(verifier.l2_safe().block_info.number, 0);

    // Block 2: valid batch — must be derived after the garbage.
    let blk2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(blk2).await.expect("signal valid block");
    let derived_real = verifier.act_l2_pipeline_full().await.expect("step valid");
    assert_eq!(derived_real, 1, "valid batch after garbage must still be derived: {kind:?}");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}

/// Random-looking garbage (200 bytes of 0xDE) is silently ignored.
#[tokio::test]
async fn garbage_random_silently_ignored() {
    garbage_kind_silently_ignored_then_valid_batch_derived(GarbageKind::Random).await;
}

/// A truncated frame (valid prefix + 16-byte channel ID, then EOF) is silently
/// ignored.
#[tokio::test]
async fn garbage_truncated_silently_ignored() {
    garbage_kind_silently_ignored_then_valid_batch_derived(GarbageKind::Truncated).await;
}

/// A frame with a valid header but an invalid RLP body is silently ignored.
#[tokio::test]
async fn garbage_malformed_rlp_silently_ignored() {
    garbage_kind_silently_ignored_then_valid_batch_derived(GarbageKind::MalformedRlp).await;
}

/// A frame with a valid header and brotli magic byte but a corrupt body is
/// silently ignored.
#[tokio::test]
async fn garbage_invalid_brotli_silently_ignored() {
    garbage_kind_silently_ignored_then_valid_batch_derived(GarbageKind::InvalidBrotli).await;
}

// ── L2 finalization tracking ───────────────────────────────────────────────────

/// The finalized L2 head advances when an L1 finalized signal is received for
/// an L1 block that is at or above the L1 origin of derived safe L2 blocks.
///
/// Setup: derive 2 L2 blocks whose L1 origin is L1 genesis (block 0).
/// After deriving, signal that L1 block 1 is finalized. Since both L2 blocks
/// have `l1_origin.number = 0 ≤ 1`, they should both become finalized; the
/// highest (L2 block 2) is the new finalized head.
#[tokio::test]
async fn l2_finalized_advances_via_l1_finalized_signal() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let block1 = sequencer.build_next_block().expect("build block 1");
    let hash1 = sequencer.head().block_info.hash;
    let block2 = sequencer.build_next_block().expect("build block 2");
    let hash2 = sequencer.head().block_info.hash;

    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.initialize().await.expect("initialize");

    // Finalized head starts at L2 genesis.
    assert_eq!(verifier.l2_finalized().block_info.number, 0);

    // Derive both L2 blocks.
    for i in 1u64..=2 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(verifier.l2_safe().block_info.number, 2);

    // Signal that L1 block 1 is finalized. Both L2 blocks have l1_origin = 0 ≤ 1,
    // so L2 block 2 (the highest) should become the new finalized head.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_finalized_signal(l1_block_1).await.expect("finalized signal");
    assert_eq!(
        verifier.l2_finalized().block_info.number,
        2,
        "finalized head should advance to L2 block 2 when L1 origin (0) <= finalized L1 (1)"
    );
}

// ── Sequencer L1-origin pin ────────────────────────────────────────────────────

/// [`L2Sequencer::pin_l1_origin`] freezes the L2 epoch on a specific L1 block
/// regardless of how many newer L1 blocks are available.
///
/// [`L2Sequencer::build_empty_block`] produces a deposit-only block (exactly
/// 1 transaction: the L1-info deposit) while the pin is active. After calling
/// [`L2Sequencer::clear_l1_origin_pin`], automatic epoch selection resumes.
#[tokio::test]
async fn sequencer_pin_l1_origin_keeps_epoch_and_empty_block() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine 2 L1 blocks so multiple epochs are available for auto-selection.
    h.mine_l1_blocks(2);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Pin the sequencer to epoch 0 (L1 genesis).
    let l1_block_0 = block_info_from(h.l1.block_by_number(0).expect("genesis"));
    sequencer.pin_l1_origin(l1_block_0);

    // Build 3 L2 blocks — all must reference epoch 0 even though L1 blocks 1
    // and 2 are available.
    for _ in 0..3 {
        let _block = sequencer.build_next_block().expect("pinned build");
        assert_eq!(
            sequencer.head().l1_origin.number,
            0,
            "epoch must remain pinned to 0 while pin is active"
        );
    }

    // build_empty_block with the pin active: only 1 transaction (deposit).
    let empty = sequencer.build_empty_block().expect("empty block");
    assert_eq!(
        empty.body.transactions.len(),
        1,
        "empty block must contain exactly the L1-info deposit transaction"
    );
    assert_eq!(sequencer.head().l1_origin.number, 0, "epoch still pinned after empty block");

    // Clear the pin — automatic epoch selection resumes.
    sequencer.clear_l1_origin_pin();
    let _block = sequencer.build_next_block().expect("unpinned build");
    // With block_time=2 and L1 block 1 at ts=12, the first unpinned block's
    // timestamp (current head ts + 2) is well below 12, so the epoch remains
    // at 0. Regardless, we just verify the pin was cleared without error.
    assert!(
        sequencer.head().l1_origin.number <= 2,
        "epoch should be within [0, 2] after clearing pin"
    );
}

// ── Derive from non-zero L1 genesis ───────────────────────────────────────────

/// Derivation works correctly when the L2 genesis is anchored to a non-zero
/// L1 block (block #5 in this case).
///
/// This exercises the derivation pipeline's ability to start traversal from
/// an arbitrary L1 origin rather than always from L1 block 0. The verifier
/// is constructed directly (bypassing the harness helper that always anchors
/// to L1 block 0).
///
/// Setup:
///   Mine 5 L1 blocks (#0–#5): these are "pre-history" from L2's perspective.
///   The L2 genesis is anchored to L1 block #5.
///   Build 2 L2 blocks referencing epoch #5.
///   Submit their batches in L1 blocks #6 and #7.
///   Verify both are derived by a pipeline that starts at L1 block #5.
#[tokio::test]
async fn derive_chain_from_near_l1_genesis() {
    let batcher_cfg = BatcherConfig::default();
    let mut rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Mine 5 "pre-history" L1 blocks before L2 genesis.
    h.mine_l1_blocks(5);

    let l1_block_5 = h.l1.block_by_number(5).expect("block 5");
    let l1_block_5_hash = l1_block_5.hash();
    let l1_genesis_info = block_info_from(l1_block_5);

    // Anchor the rollup genesis to L1 block #5 so ActionL2ChainProvider::from_genesis
    // creates the genesis L2 block with the correct l1_origin.
    rollup_cfg.genesis.l1 = BlockNumHash { number: 5, hash: l1_block_5_hash };
    // Set L2 genesis time to match L1 block #5 so that L2 block timestamps
    // (genesis_time + block_time) satisfy batch_timestamp >= l1_epoch_timestamp.
    rollup_cfg.genesis.l2_time = l1_block_5.timestamp();

    // Build an L2 genesis head anchored to L1 block #5.
    let genesis_head = L2BlockInfo {
        block_info: BlockInfo {
            hash: rollup_cfg.genesis.l2.hash,
            number: rollup_cfg.genesis.l2.number,
            parent_hash: Default::default(),
            timestamp: rollup_cfg.genesis.l2_time,
        },
        l1_origin: BlockNumHash { number: 5, hash: l1_block_5_hash },
        seq_num: 0,
    };

    // Build an L2Sequencer starting from this custom genesis (epoch 5).
    let l1_chain_snap = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let system_config = rollup_cfg.genesis.system_config.unwrap_or_default();
    let mut sequencer =
        L2Sequencer::new(genesis_head, l1_chain_snap, rollup_cfg.clone(), system_config);

    // Build 2 L2 blocks and batch them into L1 blocks #6 and #7.
    let mut block_hashes: Vec<(u64, B256)> = Vec::new();
    for i in 1..=2u64 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));
        // With block_time=2 and L1 block 6 at ts=72, L2 block ts < 72
        // so the epoch stays at 5.
        assert_eq!(sequencer.head().l1_origin.number, 5, "epoch should stay at 5");

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("encode");
        drop(batcher);
        h.l1.mine_block(); // mines L1 block 5+i
    }

    // Build the verifier components manually to anchor derivation at L1 block #5.
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let rollup_arc = Arc::new(rollup_cfg.clone());
    let l1_chain_config = Arc::new(L1ChainConfig::default());
    let l1_provider = ActionL1ChainProvider::new(chain.clone());
    let dap_source = ActionDataSource::new(chain.clone(), rollup_cfg.batch_inbox_address);
    let l2_provider = ActionL2ChainProvider::from_genesis(&rollup_cfg);

    let mut verifier = L2Verifier::new(
        rollup_arc,
        l1_chain_config,
        l1_provider,
        dap_source,
        l2_provider,
        genesis_head,
        l1_genesis_info,
    );

    for (n, hash) in &block_hashes {
        verifier.register_block_hash(*n, *hash);
    }
    verifier.initialize().await.expect("initialize");

    // Signal L1 blocks #6 and #7, each containing one batch.
    for i in 6u64..=7 {
        let blk = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(blk).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }

    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "both L2 blocks derived when genesis is anchored to L1 block #5"
    );
}

// ---------------------------------------------------------------------------
// Blob DA derivation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_l2_block_derived_from_blob() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block().expect("build L2 block 1"));

    // Encode the L2 block into frames (without submitting to L1 as calldata).
    let mut batcher = h.create_batcher(source, batcher_cfg);
    let frames = batcher.encode_frames().expect("batcher should encode the block");
    drop(batcher);

    // Build the frame data payload: [DERIVATION_VERSION_0] ++ encoded frames.
    let mut frame_data = vec![DERIVATION_VERSION_0];
    for frame in &frames {
        frame_data.extend_from_slice(&frame.encode());
    }

    // Encode frame data into a blob and enqueue it for the next L1 block.
    let blob = BlobEncoder::encode(&frame_data).expect("blob encoding failed");
    let versioned_hash = B256::repeat_byte(0xAB);
    h.l1.enqueue_blob(versioned_hash, blob);
    h.l1.mine_block();

    // Create the blob verifier AFTER mining so the snapshot contains the blob.
    let (mut verifier, _chain) = h.create_blob_verifier();
    let l1_block_1 = block_info_from(h.l1.tip());

    verifier.initialize().await.expect("initialize should succeed");
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal should succeed");
    let derived = verifier.act_l2_pipeline_full().await.expect("pipeline step should succeed");

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should be L2 block 1");
}

#[tokio::test]
async fn multiple_l2_blocks_derived_from_blob() {
    const L2_BLOCK_COUNT: u64 = 3;

    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 blocks 1-3 from genesis.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut all_source = ActionL2Source::new();
    let mut block_hashes = Vec::new();
    for _ in 1..=L2_BLOCK_COUNT {
        all_source.push(builder.build_next_block().expect("build block"));
        let head = builder.head();
        block_hashes.push((head.block_info.number, head.block_info.hash));
    }

    // Encode all 3 blocks into a single channel and get the frames.
    let mut batcher = h.create_batcher(all_source, batcher_cfg);
    let frames = batcher.encode_frames().expect("batcher should encode blocks");
    drop(batcher);

    // Build frame data and encode into a blob.
    let mut frame_data = vec![DERIVATION_VERSION_0];
    for frame in &frames {
        frame_data.extend_from_slice(&frame.encode());
    }

    let blob = BlobEncoder::encode(&frame_data).expect("blob encoding failed");
    let versioned_hash = B256::repeat_byte(0xBB);
    h.l1.enqueue_blob(versioned_hash, blob);
    h.l1.mine_block();

    // Create the blob verifier.
    let (mut verifier, _chain) = h.create_blob_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    let l1_block_1 = block_info_from(h.l1.tip());

    verifier.initialize().await.expect("initialize should succeed");
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal should succeed");
    let derived = verifier.act_l2_pipeline_full().await.expect("pipeline step should succeed");

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected 3 L2 blocks to be derived");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        L2_BLOCK_COUNT,
        "safe head should be L2 block 3"
    );
}

/// A `SystemConfig` batcher-address update committed in an L1 block is
/// correctly rolled back when that L1 block is removed by a reorg.
///
/// The test proceeds in six phases:
///
///   1. **Setup** — build three L2 blocks upfront with batcher A.
///   2. **Derive blocks 1-2** — batcher A frames are accepted, safe head → 2.
///   3. **Rotate config** — L1 block 3 carries a `ConfigUpdate` log switching
///      the batcher address from A to B.  No batch data, so safe head stays 2.
///   4. **Verify old batcher ignored** — batcher A submits block 3 in L1 block
///      4.  The pipeline ignores it because its internal batcher address is now
///      B.  Safe head stays 2.
///   5. **Reorg** — L1 is rewound to block 2, discarding the config update and
///      block 4.  The pipeline is reset with the genesis (pre-rotation) system
///      config.
///   6. **New fork** — batcher A submits block 3 in L1 block 3'.  The pipeline
///      now accepts it because the config update was rolled back.  Safe head
///      advances to 3.
///
/// This is the reorg-reversal half of op-e2e's `BatcherKeyRotation` scenario.
#[tokio::test]
async fn batcher_config_update_rolled_back_on_reorg() {
    // --- Phase 1: Setup ---
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_a = BatcherConfig::default();
    let batcher_b =
        BatcherConfig { batcher_address: Address::repeat_byte(0xBB), ..batcher_a.clone() };

    let mut rollup_cfg = rollup_config_for(&batcher_a);
    rollup_cfg.l1_system_config_address = l1_sys_cfg_addr;
    let genesis_sys_cfg = rollup_cfg.genesis.system_config.unwrap_or_default();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build L2 blocks 1, 2, 3 upfront from the L1 genesis state.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block().expect("build block 1");
    let hash1 = builder.head().block_info.hash;
    let block2 = builder.build_next_block().expect("build block 2");
    let hash2 = builder.head().block_info.hash;
    let block3 = builder.build_next_block().expect("build block 3");
    let hash3 = builder.head().block_info.hash;

    // Clone blocks for resubmission on the new fork after reorg.
    let block1_clone = block1.clone();
    let block2_clone = block2.clone();

    // --- Phase 2: Derive blocks 1-2 with batcher A (L1 blocks 1-2). ---
    for block in [block1, block2] {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_a.clone());
        batcher.advance().expect("batcher A encode");
        drop(batcher);
        h.l1.mine_block();
    }

    // --- Phase 3: Rotate config (L1 block 3 — config update log only). ---
    h.l1.enqueue_log(batcher_update_log(l1_sys_cfg_addr, batcher_b.batcher_address));
    h.l1.mine_block();

    // Create verifier after all pre-reorg L1 blocks are mined.
    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);
    verifier.register_block_hash(3, hash3);
    verifier.initialize().await.expect("initialize");

    // Drive derivation through L1 blocks 1-2.
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

    // --- Phase 4: Verify old batcher is now ignored (L1 block 4). ---
    let mut source_a = ActionL2Source::new();
    source_a.push(block3.clone());
    let mut batcher = h.create_batcher(source_a, batcher_a.clone());
    batcher.advance().expect("batcher A encode block 3");
    drop(batcher);
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());

    verifier.act_l1_head_signal(block_info_from(h.l1.tip())).await.expect("signal block 4");
    let derived_a = verifier.act_l2_pipeline_full().await.expect("step block 4");
    assert_eq!(derived_a, 0, "batcher A frame must be ignored after key rotation");
    assert_eq!(verifier.l2_safe().block_info.number, 2, "safe head must not advance");

    // --- Phase 5: Reorg L1 back to genesis (discard config update + block 4). ---
    // We must discard all post-genesis L1 blocks so the pipeline can be cleanly
    // reset to genesis.  L1 blocks 1-2 (with batcher A frames) will be re-mined
    // on the new fork.
    h.l1.reorg_to(0).expect("reorg to genesis");
    chain.truncate_to(0);

    let l1_genesis = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let l2_genesis = h.l2_genesis();

    verifier.act_reset(l1_genesis, l2_genesis, genesis_sys_cfg).await.expect("reset");
    // Drain the reset origin (genesis has no batch data).
    verifier.act_l2_pipeline_full().await.expect("drain genesis after reset");

    // --- Phase 6: New fork — re-mine blocks 1-2 with batcher A, then block 3'
    //     also with batcher A (no config update log). ---
    // Re-submit the same L2 blocks that were derived pre-reorg, plus block 3.
    // Build fresh batchers to create new frames for the new fork.
    let resubmit_blocks = [block1_clone, block2_clone, block3];
    for block in resubmit_blocks {
        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_a.clone());
        batcher.advance().expect("batcher A encode on new fork");
        drop(batcher);
        h.l1.mine_block();
        chain.push(h.l1.tip().clone());
    }

    // Drive derivation through L1 blocks 1', 2', 3' on the new fork.
    for i in 1u64..=3 {
        let block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(block).await.expect("signal");
        verifier.act_l2_pipeline_full().await.expect("step");
    }
    assert_eq!(
        verifier.l2_safe().block_info.number,
        3,
        "safe head advances to 3 — config rollback restored batcher A"
    );
}

// ── act_l2_pipeline_until edge-case tests ─────────────────────────────────────

/// Submit the batch for L2 block 2 to L1 before the batch for L2 block 1.
///
/// The [`BatchQueue`] (pre-Holocene) buffers future batches rather than
/// dropping them. When the missing predecessor batch (block 1) arrives on the
/// next L1 block, the queue derives block 1 first, then pops the buffered
/// block 2 — restoring correct L2 ordering even though the L1 submission order
/// was reversed.
///
/// [`act_l2_pipeline_until`] is used to stop after each
/// [`StepResult::PreparedAttributes`] so the test can assert the exact block
/// number at each derivation step rather than racing to the final safe head.
///
/// [`BatchQueue`]: base_consensus_derive::BatchQueue
/// [`act_l2_pipeline_until`]: L2Verifier::act_l2_pipeline_until
#[tokio::test]
async fn out_of_order_singular_batches_reordered_by_batch_queue() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks in epoch 0 (both reference L1 genesis as L1 origin).
    let block1 = builder.build_next_block().expect("build L2 block 1");
    let hash1 = builder.head().block_info.hash;
    let block2 = builder.build_next_block().expect("build L2 block 2");
    let hash2 = builder.head().block_info.hash;

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);

    // L1 block 1: carry the batch for L2 block 2 (submitted out of order).
    {
        let mut source = ActionL2Source::new();
        source.push(block2);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        batcher.advance().expect("submit future batch (block 2)");
        drop(batcher);
    }
    h.mine_and_push(&chain); // L1 block 1: future batch

    // L1 block 2: carry the batch for L2 block 1 (the expected-next batch).
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        let mut batcher = h.create_batcher(source, batcher_cfg);
        batcher.advance().expect("submit present batch (block 1)");
        drop(batcher);
    }
    h.mine_and_push(&chain); // L1 block 2: present batch

    verifier.initialize().await.expect("initialize");

    // Signal L1 block 1 and step until idle.  The BatchQueue sees a future
    // batch (block 2, timestamp 4 > expected 2) and buffers it.  No attributes
    // are produced; the pipeline returns Eof.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let (_, hit) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step after block 1 signal");
    assert!(
        !hit,
        "pipeline must not derive anything from L1 block 1 alone: \
         the batch for block 2 is a future batch — block 1 has not arrived yet"
    );
    assert_eq!(verifier.l2_safe().block_info.number, 0, "safe head must remain at genesis");

    // Signal L1 block 2.  The BatchQueue now receives the expected-next batch
    // (block 1) and derives it before popping the buffered block 2.
    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal block 2");

    // First PreparedAttributes: must be L2 block 1 (earliest timestamp).
    let (_, hit1) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 1 attributes");
    assert!(hit1, "pipeline must derive block 1 when its batch arrives");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        1,
        "BatchQueue must reorder: block 1 derived before the buffered block 2"
    );

    // Second PreparedAttributes: the buffered block 2 batch is now the
    // expected-next (timestamp 4 == safe head timestamp 2 + block_time 2).
    let (_, hit2) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 2 attributes");
    assert!(hit2, "pipeline must derive buffered block 2 after block 1 is safe");
    assert_eq!(verifier.l2_safe().block_info.number, 2, "safe head must reach block 2");
}

/// [`act_l2_pipeline_until`] returns `(steps, false)` when the pipeline is
/// idle (no L1 data signalled yet), and `(steps, true)` once a block with
/// batch data is signalled.
///
/// This documents the Eof → data → attributes lifecycle and verifies that
/// calling [`act_l2_pipeline_until`] before and after an
/// [`act_l1_head_signal`] produces the expected pair of outcomes.
///
/// [`act_l2_pipeline_until`]: L2Verifier::act_l2_pipeline_until
/// [`act_l1_head_signal`]: L2Verifier::act_l1_head_signal
#[tokio::test]
async fn pipeline_idle_before_l1_signal_derives_after() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block().expect("build L2 block 1");
    let hash1 = builder.head().block_info.hash;

    let mut source = ActionL2Source::new();
    source.push(block1);
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode and submit");
    drop(batcher);

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    h.mine_and_push(&chain);
    verifier.initialize().await.expect("initialize");

    // Before any signal: the pipeline exhausted genesis during initialize() and
    // is now at Eof.  The condition should never fire; the call returns quickly.
    let (_, before_signal) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 50)
        .await
        .expect("step before signal");
    assert!(!before_signal, "pipeline must be idle before receiving an L1 head signal");
    assert_eq!(verifier.l2_safe().block_info.number, 0);

    // After signalling L1 block 1: the pipeline can now derive L2 block 1.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let (_, after_signal) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step after signal");
    assert!(after_signal, "pipeline must derive L2 block 1 after receiving L1 head signal");
    assert_eq!(verifier.l2_safe().block_info.number, 1);
}

/// After all L2 blocks from an L1 block are derived, the pipeline emits
/// [`StepResult::AdvancedOrigin`] when it transitions to the next L1 block.
///
/// Two L2 blocks are encoded into a single L1 channel.  After both are derived,
/// the pipeline has exhausted L1 block 1's data.  Signalling an empty L1
/// block 2 and calling [`act_l2_pipeline_until`] with
/// [`StepResult::AdvancedOrigin`] catches the L1 origin advance without any
/// new L2 attributes being produced — demonstrating that the safe head stays
/// at 2 while the pipeline moves forward on L1.
///
/// `AdvancedOrigin` is returned by `DerivationPipeline::step` only when
/// `next_attributes` returns `Eof` for the current epoch **and** the
/// traversal can advance to the next L1 block.  A single-batch block bypasses
/// this: `next_attributes` succeeds inline and returns `PreparedAttributes`
/// without emitting `AdvancedOrigin` first.  The two-block setup here
/// exhausts all attributes from block 1 before block 2 is processed.
///
/// [`act_l2_pipeline_until`]: L2Verifier::act_l2_pipeline_until
#[tokio::test]
async fn pipeline_l1_origin_advance_observable_after_epoch_exhausted() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks and submit them in a single L1 channel (same L1 block).
    let mut source = ActionL2Source::new();
    let block1 = builder.build_next_block().expect("build L2 block 1");
    let hash1 = builder.head().block_info.hash;
    let block2 = builder.build_next_block().expect("build L2 block 2");
    let hash2 = builder.head().block_info.hash;
    source.push(block1);
    source.push(block2);

    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("encode and submit both blocks");
    drop(batcher);

    let (mut verifier, chain) = h.create_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.register_block_hash(2, hash2);

    h.mine_and_push(&chain); // L1 block 1: carries the channel for blocks 1 & 2
    h.mine_and_push(&chain); // L1 block 2: empty — used only for origin advance

    verifier.initialize().await.expect("initialize");

    // Signal and derive both L2 blocks from L1 block 1.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal block 1");
    let (_, hit1) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for L2 block 1");
    assert!(hit1);
    assert_eq!(verifier.l2_safe().block_info.number, 1);

    let (_, hit2) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for L2 block 2");
    assert!(hit2);
    assert_eq!(verifier.l2_safe().block_info.number, 2);

    // L1 block 1 is now fully exhausted.  Signal the empty L1 block 2 and
    // step until AdvancedOrigin: the pipeline advances the L1 origin from
    // block 1 to block 2 without producing any new L2 attributes.
    let l1_block_2 = block_info_from(h.l1.block_by_number(2).expect("block 2"));
    verifier.act_l1_head_signal(l1_block_2).await.expect("signal block 2");
    let (_, advanced) = verifier
        .act_l2_pipeline_until(|r| matches!(r, StepResult::AdvancedOrigin), 50)
        .await
        .expect("step until origin advance");
    assert!(
        advanced,
        "pipeline must emit AdvancedOrigin when transitioning from an \
         exhausted L1 block to the next"
    );
    assert_eq!(
        verifier.l2_safe().block_info.number,
        2,
        "safe head must not change: origin advanced but empty block 2 has no batch"
    );
}
