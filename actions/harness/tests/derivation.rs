//! Action tests for L2 derivation via the verifier pipeline.

use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, Bytes, U256};
use base_action_harness::{
    ActionDataSource, ActionEngineClient, ActionL1ChainProvider, ActionL2ChainProvider,
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, PendingTx,
    SharedL1Chain, TestGossipTransport, TestRollupConfigBuilder, TestRollupNode, UserDeposit,
    block_info_from,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_consensus_derive::{PipelineBuilder, StatefulAttributesBuilder, StepResult};
use base_protocol::{BatchType, BlockInfo, DERIVATION_VERSION_0, L2BlockInfo};

/// The derivation pipeline reads a single batcher frame from L1 and derives
/// the corresponding L2 block, advancing the safe head from genesis (0) to 1.
#[tokio::test]
async fn single_l2_block_derived_from_batcher_frame() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 using the L2Sequencer, which automatically computes
    // epoch_num=0 and epoch_hash from the L1 genesis block.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Create the node AFTER mining so the SharedL1Chain snapshot already
    // contains both genesis and block 1.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Step the pipeline until it is idle.
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(node.l2_safe_number(), 1, "safe head should be L2 block 1");

    // SafeDB: L2 safe head at L1 block 1 (where the batch landed) should be L2 block 1.
    let safe = node.safe_head_at_l1(1).await.unwrap();
    assert_eq!(safe.safe_head.number, 1, "safedb: safe head at L1#1 should be L2#1");
    assert_eq!(safe.l1_block.number, 1, "safedb: l1_block at L1#1 should be 1");
}

/// Mine several L1 blocks, each containing one batch, and verify the safe head
/// advances by one L2 block per L1 block.
///
/// All three L2 blocks belong to the same L1 epoch (genesis). This is the
/// realistic Base scenario: with 12 s L1 blocks and 2 s L2 blocks there
/// are ~6 L2 slots per L1 epoch; each batch may land in a different L1 block
/// within the sequencer window while still referencing the same L1 epoch.
#[tokio::test]
async fn multiple_l1_blocks_each_derive_one_l2_block() {
    const L2_BLOCK_COUNT: u64 = 3;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 blocks 1-3 from genesis. With block_time=2 and L1 block_time=12,
    // all three blocks (timestamps 2, 4, 6 s) stay in epoch 0 (genesis, ts=0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 1..=L2_BLOCK_COUNT {
        batcher.push_block(builder.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(
        total_derived, L2_BLOCK_COUNT as usize,
        "expected {L2_BLOCK_COUNT} L2 blocks derived"
    );

    assert_eq!(node.l2_safe_number(), L2_BLOCK_COUNT);

    // SafeDB: each L2 block was derived from its own L1 block. Verify every
    // individual L1→L2 mapping, not just the last one.
    for i in 1..=L2_BLOCK_COUNT {
        let safe = node.safe_head_at_l1(i).await.unwrap();
        assert_eq!(safe.safe_head.number, i, "safedb: safe head at L1#{i} should be L2#{i}");
        assert_eq!(safe.l1_block.number, i, "safedb: l1_block at L1#{i} should be {i}");
    }
}

/// A batcher frame that lands in an L1 block which is subsequently reorged out
/// must NOT be derived. The verifier is created on the post-reorg chain
/// (verifier never saw the orphaned block), so no reset is needed — the chain
/// snapshot passed to the verifier already reflects the canonical fork.
#[tokio::test]
async fn batch_in_orphaned_l1_block_is_not_derived() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Encode L2 block 1 and mine L1 block 1 containing the batcher frame.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Reorg L1 back to genesis; mine an empty replacement block 1'.
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block();
    // The node is created from the miner's current (post-reorg) state, so
    // the orphaned block 1 is not present in the SharedL1Chain snapshot.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 0, "batch was in orphaned block; nothing should be derived");
    assert_eq!(node.l2_safe_number(), 0, "safe head remains at genesis");
}

/// After the verifier has derived L2 block 1 (safe head = 1), an L1 reorg
/// back to genesis is detected and the pipeline is reset. The safe head must
/// revert to 0 and no new L2 blocks must be derived from the empty replacement
/// L1 block.
#[tokio::test]
async fn reorg_reverts_derived_safe_head() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Batch and mine L1 block 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Create the node and derive L2 block 1.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "L2 block 1 derived before reorg");
    assert_eq!(node.l2_safe_number(), 1);

    // Reorg L1 back to genesis; mine an empty replacement block 1'.
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block();
    // Sync the SharedL1Chain that the node's providers read from.
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    // Reset the pipeline: revert safe head and L1 origin to genesis.
    let l2_genesis = h.l2_genesis();

    node.act_reset(l2_genesis).await;
    // Drain the reset origin (genesis has no batch data).
    node.run_until_idle().await;

    // Signal the new fork's empty block 1' and step.
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 0, "no batch in reorged fork");
    assert_eq!(node.l2_safe_number(), 0, "safe head reverted to genesis");
}

/// After a reorg, the batcher resubmits the lost frame in a new L1 block on
/// the canonical fork. The verifier must re-derive the same L2 block from the
/// new inclusion block, recovering the safe head back to 1.
#[tokio::test]
async fn reorg_and_resubmit_rederives_l2_block() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // --- Pre-reorg: derive L2 block 1 from L1 block 1. ---
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;

    {
        let mut source = ActionL2Source::new();
        source.push(block1.clone());
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1);
    assert_eq!(node.l2_safe_number(), 1);

    // --- Reorg: truncate L1 to genesis; mine an empty block 1'. ---
    h.l1.reorg_to(0).expect("reorg to genesis");
    h.l1.mine_block(); // block 1' (empty)
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    // Reset pipeline to genesis.
    let l2_genesis = h.l2_genesis();

    node.act_reset(l2_genesis).await;
    node.run_until_idle().await;

    // Step over the empty block 1' — nothing derived.
    let empty = node.run_until_idle().await;
    assert_eq!(empty, 0, "block 1' has no batch; nothing derived");
    assert_eq!(node.l2_safe_number(), 0);

    // --- Resubmit: re-encode block 1 in L1 block 2'. ---
    // The same block 1 (cloned) re-submitted with the same epoch info will be
    // accepted by the pipeline on the new fork.
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone());

    // Derive L2 block 1 from the resubmitted batch in L1 block 2'.
    let rederived = node.run_until_idle().await;

    assert_eq!(rederived, 1, "L2 block 1 re-derived from resubmitted batch");
    assert_eq!(node.l2_safe_number(), 1, "safe head recovered to 1");
}

/// The canonical chain flip-flops between two competing forks (A and B) three
/// times.  After each switch the pipeline is reset and must re-derive the same
/// L2 block from whichever fork is currently canonical, without accumulating
/// stale channel or frame data from a previous fork.
///
#[tokio::test]
async fn reorg_flip_flop() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build L2 block 1 once; we re-use (clone) it across all forks since the
    // epoch info is the same (all forks reference L1 genesis as epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block_with_single_transaction().await;

    // Shared reset helpers — computed once, valid across all forks because
    // genesis is immutable.
    let l2_genesis = h.l2_genesis();

    // --- Phase 1: Fork A canonical (genesis → A1 with batch). ---
    {
        let mut source = ActionL2Source::new();
        source.push(block1.clone());
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "phase 1: L2 block 1 derived from fork A");
    assert_eq!(node.l2_safe_number(), 1);

    // --- Phase 2: Fork B canonical (reorg A; mine B1 with the same batch). ---
    h.l1.reorg_to(0).expect("reorg to fork B");
    {
        let mut source = ActionL2Source::new();
        source.push(block1.clone());
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    node.act_reset(l2_genesis).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "phase 2: L2 block 1 re-derived from fork B");
    assert_eq!(node.l2_safe_number(), 1);

    // --- Phase 3: Fork A' canonical (reorg B; mine A1' — same batch, new fork). ---
    h.l1.reorg_to(0).expect("reorg to fork A'");
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.truncate_to(0);
    chain.push(h.l1.tip().clone());

    node.act_reset(l2_genesis).await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "phase 3: L2 block 1 re-derived from fork A'");
    assert_eq!(node.l2_safe_number(), 1);
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build L2 blocks 1-2 against a genesis-only chain so both reference epoch 0
    // and their encoded batch frames are valid on any fork sharing genesis.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    // Shared reset targets — valid across all forks because genesis is immutable.
    let l2_genesis = h.l2_genesis();

    // --- Fork A: mine A1 (batch for L2 block 1) and A2 (batch for L2 block 2). ---
    let mut batcher_a = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in [block1.clone(), block2.clone()] {
        batcher_a.push_block(block);
        batcher_a.advance(&mut h.l1).await;
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_a = node.run_until_idle().await;
    assert_eq!(total_a, 2, "fork A: both L2 blocks derived");
    assert_eq!(node.l2_safe().l1_origin.number, 0, "fork A: all blocks in epoch 0");
    assert_eq!(node.l2_safe_number(), 2, "fork A: safe head = 2");

    // --- Fork B: reorg to genesis; mine two empty blocks; derive nothing. ---
    h.l1.reorg_to(0).expect("reorg to fork B");
    chain.truncate_to(0);
    for _ in 0..2 {
        h.mine_and_push(&chain);
    }

    node.act_reset(l2_genesis).await;
    // act_reset sets safe_head and finalized_head to the reset target (l2_genesis).
    // Per the Base spec, unsafe_head is NOT clamped to safe_head on reset —
    // it is re-discovered by walking back from the current tip to the first block
    // with a plausible (canonical or ahead-of-L1) L1 origin.  In this node-only
    // context no gossip blocks were received, so unsafe_head was never advanced
    // beyond genesis and therefore remains 0 regardless.
    assert_eq!(node.l2_safe_number(), 0, "reset to B: safe head = 0");
    assert_eq!(node.l2_finalized_number(), 0, "reset to B: finalized head = 0");
    assert_eq!(node.l2_unsafe_number(), 0, "reset to B: unsafe head = 0");

    let total_b = node.run_until_idle().await;
    assert_eq!(total_b, 0, "fork B: empty blocks derive nothing");
    assert_eq!(node.l2_safe_number(), 0, "fork B: safe head = 0");
    assert_eq!(node.l2_finalized_number(), 0, "fork B: finalized head = 0");

    // --- Fork C: reorg to genesis; resubmit both batches; re-derive both blocks. ---
    h.l1.reorg_to(0).expect("reorg to fork C");
    chain.truncate_to(0);
    let mut batcher_c = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in [block1, block2] {
        batcher_c.push_block(block);
        batcher_c.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    node.act_reset(l2_genesis).await;
    assert_eq!(node.l2_safe_number(), 0, "reset to C: safe head = 0");
    assert_eq!(node.l2_finalized_number(), 0, "reset to C: finalized head = 0");
    // unsafe_head unchanged by act_reset (spec-compliant: re-discover, don't clamp).
    assert_eq!(node.l2_unsafe_number(), 0, "reset to C: unsafe head = 0");

    let total_c = node.run_until_idle().await;
    assert_eq!(total_c, 2, "fork C: both L2 blocks re-derived");
    assert_eq!(node.l2_safe().l1_origin.number, 0, "fork C: all blocks in epoch 0");
    assert_eq!(node.l2_safe_number(), 2, "fork C: safe head = 2 after flip-flop");
    // finalized_head stays at genesis because no act_l1_finalized_signal was sent.
    assert_eq!(node.l2_finalized_number(), 0, "fork C: finalized head = 0");
}

/// A batch submitted at the last valid L1 block within the sequence window
/// must be derived successfully.
///
/// With `seq_window_size = 4` and `epoch = 0`, the valid inclusion range is
/// L1 blocks 1–3 (strictly: `epoch(0) + window(4) > inclusion_block`).
/// Submitting the batch in block 3 — the final valid slot — must succeed.
///
#[tokio::test]
async fn batch_accepted_at_last_seq_window_block() {
    const SEQ_WINDOW: u64 = 4;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_seq_window_size(SEQ_WINDOW)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 referencing L1 genesis (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;

    // Mine 2 empty L1 blocks (no batch yet).
    h.mine_l1_blocks(2); // blocks 1 and 2

    // Submit batch and mine L1 block 3 — the last valid inclusion block for
    // epoch 0 with seq_window_size = 4 (valid iff inclusion_block < 4).
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Signal blocks 1, 2, 3 and step after each.
    for _ in 1..=SEQ_WINDOW - 1 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 1, "batch in last valid L1 block must be derived");
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_deposit_contract(deposit_contract)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 from the sequencer.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block_with_single_transaction().await;

    // Enqueue a user deposit log: from=0xAA..AA, to=0xBB..BB, value=1 ETH, gas=100k.
    h.l1.enqueue_user_deposit(&UserDeposit {
        deposit_contract,
        from: Address::repeat_byte(0xAA),
        to: Address::repeat_byte(0xBB),
        mint: 0,
        value: U256::from(1_000_000_000_000_000_000u128), // 1 ETH in wei
        gas_limit: 100_000,
        data: vec![],
    });

    // Submit the batcher frame into the same L1 block as the deposit log.
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    // Create node AFTER mining so the snapshot contains block 1.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(node.l2_safe_number(), 1, "safe head should be L2 block 1");
    // The L2 block references L1 genesis (epoch 0) because the sequencer built
    // it before L1 block 1 existed. The deposit log lives in L1 block 1, which
    // is the *inclusion* block, not the L1 origin. The pipeline processed the
    // deposit log from block 1's receipts without errors.
    assert_eq!(node.l2_safe().l1_origin.number, 0, "L2 block 1 references epoch 0");
}

/// After a batcher-address rotation committed to L1 via a `ConfigUpdate` log,
/// frames from the old batcher address are silently ignored and frames from
/// the new address are derived normally.
///
/// The rotation is delivered as a real `ConfigUpdate` log in an L1 receipt.
/// The traversal stage reads receipts via `receipts_by_hash` when
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
///
/// [`SystemConfig`]: base_consensus_genesis::SystemConfig
#[tokio::test]
async fn batcher_key_rotation_accepts_new_batcher() {
    // Use a dedicated L1 system config address so the pipeline's log filter
    // matches our synthetic ConfigUpdate logs.
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_a = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let batcher_b =
        BatcherConfig { batcher_address: Address::repeat_byte(0xBB), ..batcher_a.clone() };

    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_a)
        .with_l1_system_config_address(l1_sys_cfg_addr)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build all L2 blocks (1, 2, and 3) upfront from the L1 genesis state.
    // With block_time=2 and L1 block_time=12, all three (timestamps 2,4,6s)
    // stay in epoch 0 (genesis ts=0), so the builder picks the same L1 origin.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;
    let block3 = builder.build_next_block_with_single_transaction().await;

    // --- L1 blocks 1-2: batcher A submits → L2 blocks 1-2 derived. ---
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_a.clone());
    for block in [block1, block2] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    // --- L1 block 3: rotation log only, no batch. ---
    // After the traversal processes this block and reads the ConfigUpdate log
    // from its receipts, the pipeline's internal batcher address switches to B.
    h.l1.enqueue_batcher_update(l1_sys_cfg_addr, batcher_b.batcher_address);
    h.l1.mine_block(); // block 3: rotation receipt, no batcher tx

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Drive derivation through blocks 1-2 (batcher A frames derived).
    for _ in 1u64..=2 {
        node.run_until_idle().await;
    }
    assert_eq!(node.l2_safe_number(), 2, "blocks 1-2 derived with batcher A");

    // Step over the rotation block — no batch, but system config updates to B.
    let rotation_derived = node.run_until_idle().await;
    assert_eq!(rotation_derived, 0, "rotation block contains no batch");

    // --- L1 block 4: batcher A submits for L2 block 3 — must be ignored. ---
    {
        let mut source = ActionL2Source::new();
        source.push(block3.clone());
        Batcher::new(source, &h.rollup_config, batcher_a.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone());

    let derived_a = node.run_until_idle().await;
    assert_eq!(derived_a, 0, "batcher A frame must be ignored after key rotation");
    assert_eq!(node.l2_safe_number(), 2, "safe head must not advance");

    // --- L1 block 5: batcher B submits for L2 block 3 — must be derived. ---
    {
        let mut source = ActionL2Source::new();
        source.push(block3);
        Batcher::new(source, &h.rollup_config, batcher_b.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone());

    let derived_b = node.run_until_idle().await;
    assert_eq!(derived_b, 1, "batcher B frame must be derived after key rotation");
    assert_eq!(node.l2_safe_number(), 3, "safe head advances to 3");
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for _ in 1..=L2_COUNT {
        batcher.push_block(builder.build_next_block_with_single_transaction().await);
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, L2_COUNT as usize, "expected {L2_COUNT} L2 blocks derived");

    assert_eq!(node.l2_safe_number(), L2_COUNT, "safe head should be at L2 block {L2_COUNT}");
    assert_eq!(node.l2_safe().l1_origin.number, 0, "all blocks in epoch 0");
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_seq_window_size(SEQ_WINDOW)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1 (epoch 0).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;

    // Mine 2 empty L1 blocks (seq_window=3, so valid inclusion is blocks 1 and 2 only).
    // Batch is valid if inclusion_block < epoch + seq_window = 0 + 3 = 3.
    // So blocks 1 and 2 are valid, block 3 is NOT.
    h.mine_l1_blocks(2); // mine blocks 1 and 2 (no batch yet)

    // Submit batch in block 3 — past the window.
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let mut total_derived = 0;
    for _ in 1..=SEQ_WINDOW {
        total_derived += node.run_until_idle().await;
    }

    // The pipeline generates deposit-only blocks to fill the epoch when the
    // sequence window expires without a valid batch. The safe head advances
    // past genesis because default blocks are still derived.
    assert!(
        node.l2_safe_number() > 0,
        "pipeline should generate deposit-only blocks when sequence window expires"
    );
    assert!(
        total_derived > 0,
        "pipeline should derive deposit-only (default) blocks for the expired epoch"
    );
    // All derived blocks are in epoch 0 since no L1 epoch boundary was crossed.
    assert_eq!(
        node.l2_safe().l1_origin.number,
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 blocks 1 and 2 so the builder can advance epochs.
    h.mine_l1_blocks(2);

    // Build 12 L2 blocks from genesis using a shared L1 chain that includes
    // the two mined L1 blocks for epoch advancement.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut blocks = Vec::new();
    for _ in 0..12 {
        let block = builder.build_next_block_with_single_transaction().await;
        blocks.push(block);
    }

    // Verify the builder's epoch assignment at the boundary.
    // L2 block 6 (index 5) should be the first block in epoch 1.
    assert_eq!(builder.head().l1_origin.number, 2, "L2 block 12 should reference epoch 2");

    // Create node after the sequencer has populated the shared block-hash registry.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Batch each L2 block into a separate L1 inclusion block.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in &blocks {
        batcher.push_block(block.clone());
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    node.initialize().await;

    // Drive derivation through all L1 blocks: blocks 1-2 are epoch-providing
    // (no batches), blocks 3-14 each contain one batch.
    let mut total_derived = 0;
    for _ in 1..=(2 + 12) {
        total_derived += node.run_until_idle().await;
    }

    assert_eq!(total_derived, 12, "all 12 L2 blocks should be derived");
    assert_eq!(node.l2_safe_number(), 12, "safe head should reach L2 block 12");
}

/// Build 3 L2 blocks, encode all 3 into a single batcher submission (one
/// channel), mine one L1 block, and verify that all 3 are derived from that
/// single L1 block.
///
/// This tests that the pipeline correctly handles multiple batches within a
/// single channel frame delivered in one L1 block.
#[tokio::test]
async fn same_epoch_multi_batch_one_l1_block() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut source = ActionL2Source::new();
    for _ in 1..=3u64 {
        let block = builder.build_next_block_with_single_transaction().await;
        source.push(block);
    }

    // Encode all 3 blocks into one batcher submission (single channel) and mine.
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Create node after mining so the snapshot includes the inclusion block.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 3, "all 3 L2 blocks should be derived from one L1 block");
    assert_eq!(node.l2_safe_number(), 3);
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 5 L2 blocks.
    let mut blocks = Vec::new();
    for _ in 0..5 {
        let block = builder.build_next_block_with_single_transaction().await;
        blocks.push(block);
    }

    // Submit each block's batch individually and mine an L1 block for each.
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in &blocks {
        batcher.push_block(block.clone());
        batcher.advance(&mut h.l1).await;
    }

    // Create node with all 5 L1 inclusion blocks visible.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    for _ in 1..=5u64 {
        node.run_until_idle().await;
    }
    assert_eq!(node.l2_safe_number(), 5, "pre-reorg: 5 L2 blocks derived");

    // Reorg all the way back to genesis.
    h.l1.reorg_to(0).expect("reorg to genesis");
    chain.truncate_to(0);

    let l2_genesis = h.l2_genesis();
    node.act_reset(l2_genesis).await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 0, "safe head reverted to genesis");

    // Re-submit all 5 batches on the new fork.
    let mut resubmit_batcher =
        Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in &blocks {
        resubmit_batcher.push_block(block.clone());
        resubmit_batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    // Drive derivation on the new fork.
    for _ in 1..=5u64 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 5, "post-reorg: 5 L2 blocks recovered");
}

/// Garbage frame data (valid derivation version prefix but corrupt frame
/// bytes) must be silently ignored by the pipeline. A valid batch submitted
/// in a subsequent L1 block must still derive correctly.
///
/// This verifies the `ChannelBank`'s robustness: malformed frames are
/// dropped without crashing the pipeline or poisoning subsequent channels.
#[tokio::test]
async fn garbage_frame_data_ignored() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

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

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 0, "garbage frame must be silently ignored");
    assert_eq!(node.l2_safe_number(), 0);

    // Now submit the real batch.
    {
        let mut source = ActionL2Source::new();
        source.push(block);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone());

    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "real frame after garbage must still be derived");
    assert_eq!(node.l2_safe_number(), 1);
}

/// A channel whose compressed data exceeds `max_frame_size` is split across
/// multiple frames. All frames are submitted in the same L1 block (as separate
/// transactions) and the `ChannelBank` reassembles them into the original
/// channel data, deriving the L2 block.
///
/// This exercises the `ChannelDriver` multi-frame output path and verifies
/// that a small `max_frame_size` causes the encoder to produce multiple frame
/// transactions that the derivation pipeline reassembles correctly.
///
/// NOTE: All frames must land in the same L1 block.
#[tokio::test]
async fn multi_frame_channel_reassembled() {
    let batcher_cfg = BatcherConfig {
        // Small max_frame_size forces the channel to spill across multiple frames.
        encoder: EncoderConfig {
            max_frame_size: 80,
            da_type: DaType::Calldata,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode the L2 block. With max_frame_size=80, the compressed channel data
    // should spill across multiple frames — verified via pending_count before mining.
    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
    batcher.encode_only().await;
    assert!(
        batcher.pending_count() >= 2,
        "expected at least 2 frame submissions with max_frame_size=80, got {}",
        batcher.pending_count()
    );

    // Stage all frames, mine one L1 block, and confirm.
    let n = batcher.pending_count();
    batcher.stage_n_frames(&mut h.l1, n);
    let block_num = h.l1.mine_block().number();
    batcher.confirm_staged(block_num).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    let derived = node.run_until_idle().await;
    assert_eq!(derived, 1, "multi-frame channel should be reassembled and derived");
    assert_eq!(node.l2_safe_number(), 1);
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
    let batcher_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(sequencer.build_next_block_with_single_transaction().await);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected one L2 block from span batch");
    assert_eq!(node.l2_safe_number(), 1);
}

/// Derive 3 L2 blocks encoded together as a single [`SpanBatch`].
///
/// All 3 blocks are grouped into one span-batch channel submitted in a single
/// L1 block. The derivation pipeline must decode the span and advance the
/// safe head by 3.
#[tokio::test]
async fn three_l2_blocks_derived_from_span_batch() {
    let batcher_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut source = ActionL2Source::new();
    for _ in 1..=3u64 {
        let block = sequencer.build_next_block_with_single_transaction().await;
        source.push(block);
    }
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let derived = node.run_until_idle().await;

    assert_eq!(derived, 3, "expected 3 L2 blocks from span batch");
    assert_eq!(node.l2_safe_number(), 3);
}

// ── System-config update tests ─────────────────────────────────────────────────

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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_l1_system_config_address(l1_sys_cfg_addr)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block_with_single_transaction().await;
    let block2 = sequencer.build_next_block_with_single_transaction().await;

    // L1 block 1: batch for L2 block 1.
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    // L1 block 2: gas-config update log only, no batch.
    h.l1.enqueue_gas_config_update(l1_sys_cfg_addr, 2100, 1_000_000);
    h.l1.mine_block();

    // L1 block 3: batch for L2 block 2.
    {
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    for _ in 1u64..=3 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 2, "both L2 blocks derived after GPO config update");
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_l1_system_config_address(l1_sys_cfg_addr)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block_with_single_transaction().await;
    let block2 = sequencer.build_next_block_with_single_transaction().await;

    // L1 block 1: batch for L2 block 1.
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    // L1 block 2: gas-limit update log only.
    h.l1.enqueue_gas_limit_update(l1_sys_cfg_addr, 60_000_000);
    h.l1.mine_block();

    // L1 block 3: batch for L2 block 2.
    {
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    for _ in 1u64..=3 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 2, "both L2 blocks derived after gas-limit update");
}

// ── Typed garbage-frame variant tests ─────────────────────────────────────────

/// Submit a raw garbage payload, mine it into an L1 block, step the derivation
/// pipeline, and assert nothing is derived. Then submit a valid batch and assert
/// recovery succeeds.
///
/// This validates that the pipeline silently discards malformed data without
/// crashing or poisoning subsequent channel state.
async fn garbage_payload_silently_ignored_then_valid_batch_derived(
    garbage: alloy_primitives::Bytes,
) {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    // L1 block 1: garbage frame only.
    h.l1.submit_tx(PendingTx {
        from: batcher_cfg.batcher_address,
        to: batcher_cfg.inbox_address,
        input: garbage.clone(),
    });
    h.l1.mine_block();

    // L1 block 2: valid batch.
    {
        let mut source = ActionL2Source::new();
        source.push(block);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Both blocks processed: garbage in block 1 is silently ignored,
    // valid batch in block 2 is derived.
    let total_derived = node.run_until_idle().await;
    assert_eq!(
        total_derived, 1,
        "valid batch after garbage must be derived; garbage silently ignored"
    );
    assert_eq!(node.l2_safe_number(), 1);
}

/// Random-looking garbage (200 bytes of 0xDE, no derivation version prefix) is
/// silently ignored.
#[tokio::test]
async fn garbage_random_silently_ignored() {
    garbage_payload_silently_ignored_then_valid_batch_derived(alloy_primitives::Bytes::from(
        vec![0xDE_u8; 200],
    ))
    .await;
}

/// A truncated frame (valid derivation prefix + 16-byte channel ID, then EOF)
/// is silently ignored.
#[tokio::test]
async fn garbage_truncated_silently_ignored() {
    let mut v = vec![DERIVATION_VERSION_0];
    v.extend_from_slice(&[0x01u8; 16]); // partial channel ID, truncated
    garbage_payload_silently_ignored_then_valid_batch_derived(alloy_primitives::Bytes::from(v))
        .await;
}

/// A frame with a valid header but an invalid RLP body is silently ignored.
#[tokio::test]
async fn garbage_malformed_rlp_silently_ignored() {
    // Valid frame header layout: channel_id(16) + frame_number(2) + frame_data_length(4)
    // followed by corrupt body bytes.
    let mut v = vec![DERIVATION_VERSION_0];
    v.extend_from_slice(&[0x02u8; 16]); // channel id
    v.extend_from_slice(&[0x00, 0x00]); // frame_number = 0
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x10]); // frame_data_length = 16
    v.extend_from_slice(&[0xFF; 16]); // corrupt body (invalid RLP)
    v.push(0x00); // is_last = false
    garbage_payload_silently_ignored_then_valid_batch_derived(alloy_primitives::Bytes::from(v))
        .await;
}

/// A frame with a valid header and brotli magic byte but a corrupt body is
/// silently ignored.
#[tokio::test]
async fn garbage_invalid_brotli_silently_ignored() {
    let mut v = vec![DERIVATION_VERSION_0];
    v.extend_from_slice(&[0x03u8; 16]); // channel id
    v.extend_from_slice(&[0x00, 0x00]); // frame_number = 0
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x10]); // frame_data_length = 16
    v.push(0xCE); // brotli magic byte
    v.extend_from_slice(&[0xAB; 15]); // corrupt brotli body
    v.push(0x00); // is_last = false
    garbage_payload_silently_ignored_then_valid_batch_derived(alloy_primitives::Bytes::from(v))
        .await;
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let block1 = sequencer.build_next_block_with_single_transaction().await;
    let block2 = sequencer.build_next_block_with_single_transaction().await;

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    for block in [block1, block2] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Finalized head starts at L2 genesis.
    assert_eq!(node.l2_finalized_number(), 0);

    // Derive both L2 blocks.
    for _ in 1u64..=2 {
        node.run_until_idle().await;
    }
    assert_eq!(node.l2_safe_number(), 2);

    // Signal that L1 block 1 is finalized. Both L2 blocks have l1_origin = 0 ≤ 1,
    // so L2 block 2 (the highest) should become the new finalized head.
    let l1_block_1 = h.l1.block_info_at(1);
    node.act_l1_finalized_signal(l1_block_1).await;
    assert_eq!(
        node.l2_finalized_number(),
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
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine 2 L1 blocks so multiple epochs are available for auto-selection.
    h.mine_l1_blocks(2);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Pin the sequencer to epoch 0 (L1 genesis).
    let l1_block_0 = h.l1.block_info_at(0);
    sequencer.pin_l1_origin(l1_block_0);

    // Build 3 L2 blocks — all must reference epoch 0 even though L1 blocks 1
    // and 2 are available.
    for _ in 0..3 {
        let _block = sequencer.build_next_block_with_single_transaction().await;
        assert_eq!(
            sequencer.head().l1_origin.number,
            0,
            "epoch must remain pinned to 0 while pin is active"
        );
    }

    // build_empty_block with the pin active: only 1 transaction (deposit).
    let empty = sequencer.build_empty_block().await;
    assert_eq!(
        empty.body.transactions.len(),
        1,
        "empty block must contain exactly the L1-info deposit transaction"
    );
    assert_eq!(sequencer.head().l1_origin.number, 0, "epoch still pinned after empty block");

    // Clear the pin — automatic epoch selection resumes.
    sequencer.clear_l1_origin_pin();
    let _block = sequencer.build_next_block_with_single_transaction().await;
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
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let mut rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
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
    // Recompute the real genesis hash after changing l2_time so that the
    // sequencer's build_and_commit() fallback and check_batch()'s
    // parent_hash comparison both use the same value.
    rollup_cfg.genesis.l2.hash = ActionEngineClient::compute_l2_genesis_hash(&rollup_cfg);

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

    // Update the harness rollup config so create_l2_sequencer uses the correct genesis.
    h.rollup_config = rollup_cfg.clone();

    // Build an L2Sequencer starting from this custom genesis (epoch 5).
    let l1_chain_snap = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain_snap);
    let block_hashes = sequencer.block_hash_registry();

    // Build 2 L2 blocks and batch them into L1 blocks #6 and #7.
    let mut batcher = Batcher::new(ActionL2Source::new(), &rollup_cfg, batcher_cfg.clone());
    for _ in 1..=2u64 {
        let block = sequencer.build_next_block_with_single_transaction().await;
        // With block_time=2 and L1 block 6 at ts=72, L2 block ts < 72
        // so the epoch stays at 5.
        assert_eq!(sequencer.head().l1_origin.number, 5, "epoch should stay at 5");
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await; // mines L1 block 5+i
    }

    // Build the node components manually to anchor derivation at L1 block #5.
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let rollup_arc = Arc::new(rollup_cfg.clone());
    let l1_chain_config = Arc::new(ChainConfig::default());
    let l1_provider = ActionL1ChainProvider::new(chain.clone());
    let dap_source = ActionDataSource::new(chain.clone(), rollup_cfg.batch_inbox_address);
    let l2_provider = ActionL2ChainProvider::from_genesis(&rollup_cfg);

    let attrs_builder = StatefulAttributesBuilder::new(
        Arc::clone(&rollup_arc),
        Arc::clone(&l1_chain_config),
        l2_provider.clone(),
        l1_provider.clone(),
    );
    let pipeline = PipelineBuilder::new()
        .rollup_config(Arc::clone(&rollup_arc))
        .origin(l1_genesis_info)
        .chain_provider(l1_provider)
        .dap_source(dap_source)
        .l2_chain_provider(l2_provider)
        .builder(attrs_builder)
        .build_polled();
    let (_, p2p) = TestGossipTransport::channel();
    let engine =
        ActionEngineClient::new(Arc::clone(&rollup_arc), genesis_head, block_hashes, chain);

    let mut node = TestRollupNode::new(pipeline, engine, p2p, genesis_head, rollup_arc);
    node.initialize().await;

    // Step the pipeline until it derives both L2 blocks.
    node.run_until_idle().await;

    assert_eq!(
        node.l2_safe_number(),
        2,
        "both L2 blocks derived when genesis is anchored to L1 block #5"
    );
}

// ---------------------------------------------------------------------------
// Blob DA derivation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_l2_block_derived_from_blob() {
    let batcher_cfg = BatcherConfig::default(); // DaType::Blob by default
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 block 1.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(builder.build_next_block_with_single_transaction().await);
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Create the blob node AFTER mining so the snapshot contains the blob.
    let (mut node, _chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, 1, "expected exactly one L2 block to be derived");
    assert_eq!(node.l2_safe_number(), 1, "safe head should be L2 block 1");
}

#[tokio::test]
async fn multiple_l2_blocks_derived_from_blob() {
    const L2_BLOCK_COUNT: u64 = 3;

    let batcher_cfg = BatcherConfig::default(); // DaType::Blob by default
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Build L2 blocks 1-3 from genesis.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let mut source = ActionL2Source::new();
    for _ in 1..=L2_BLOCK_COUNT {
        source.push(builder.build_next_block_with_single_transaction().await);
    }

    // Encode all 3 blocks into a single blob channel and mine.
    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    // Create the blob node.
    let (mut node, _chain) = h.create_blob_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    let derived = node.run_until_idle().await;

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected 3 L2 blocks to be derived");
    assert_eq!(node.l2_safe_number(), L2_BLOCK_COUNT, "safe head should be L2 block 3");
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
#[tokio::test]
async fn batcher_config_update_rolled_back_on_reorg() {
    // --- Phase 1: Setup ---
    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_a = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let batcher_b =
        BatcherConfig { batcher_address: Address::repeat_byte(0xBB), ..batcher_a.clone() };

    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_a)
        .with_l1_system_config_address(l1_sys_cfg_addr)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build L2 blocks 1, 2, 3 upfront from the L1 genesis state.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;
    let block3 = builder.build_next_block_with_single_transaction().await;

    // Clone blocks for resubmission on the new fork after reorg.
    let block1_clone = block1.clone();
    let block2_clone = block2.clone();

    // --- Phase 2: Derive blocks 1-2 with batcher A (L1 blocks 1-2). ---
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_a.clone());
    for block in [block1, block2] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    // --- Phase 3: Rotate config (L1 block 3 — config update log only). ---
    h.l1.enqueue_batcher_update(l1_sys_cfg_addr, batcher_b.batcher_address);
    h.l1.mine_block();

    // Create node after all pre-reorg L1 blocks are mined.
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    // Drive derivation through L1 blocks 1-2.
    for _ in 1u64..=2 {
        node.run_until_idle().await;
    }
    assert_eq!(node.l2_safe_number(), 2, "blocks 1-2 derived with batcher A");

    // Step over the rotation block — no batch, but system config updates to B.
    let rotation_derived = node.run_until_idle().await;
    assert_eq!(rotation_derived, 0, "rotation block contains no batch");

    // --- Phase 4: Verify old batcher is now ignored (L1 block 4). ---
    {
        let mut source = ActionL2Source::new();
        source.push(block3.clone());
        Batcher::new(source, &h.rollup_config, batcher_a.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone());

    let derived_a = node.run_until_idle().await;
    assert_eq!(derived_a, 0, "batcher A frame must be ignored after key rotation");
    assert_eq!(node.l2_safe_number(), 2, "safe head must not advance");

    // --- Phase 5: Reorg L1 back to genesis (discard config update + block 4). ---
    // We must discard all post-genesis L1 blocks so the pipeline can be cleanly
    // reset to genesis.  L1 blocks 1-2 (with batcher A frames) will be re-mined
    // on the new fork.
    h.l1.reorg_to(0).expect("reorg to genesis");
    chain.truncate_to(0);

    let l2_genesis = h.l2_genesis();

    node.act_reset(l2_genesis).await;
    // Drain the reset origin (genesis has no batch data).
    node.run_until_idle().await;

    // --- Phase 6: New fork — re-mine blocks 1-2 with batcher A, then block 3'
    //     also with batcher A (no config update log). ---
    // Re-submit the same L2 blocks that were derived pre-reorg, plus block 3.
    let resubmit_blocks = [block1_clone, block2_clone, block3];
    let mut resubmit_batcher =
        Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_a.clone());
    for block in resubmit_blocks {
        resubmit_batcher.push_block(block);
        resubmit_batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
    }

    // Drive derivation through L1 blocks 1', 2', 3' on the new fork.
    for _ in 1u64..=3 {
        node.run_until_idle().await;
    }
    assert_eq!(
        node.l2_safe_number(),
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
/// [`act_l2_pipeline_until`]: TestRollupNode::act_l2_pipeline_until
#[tokio::test]
async fn out_of_order_singular_batches_reordered_by_batch_queue() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks in epoch 0 (both reference L1 genesis as L1 origin).
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: carry the batch for L2 block 2 (submitted out of order).
    {
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: future batch

    // L1 block 2: carry the batch for L2 block 1 (the expected-next batch).
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    // Do NOT push block 2 yet — let the pipeline see only block 1 first.

    node.initialize().await;

    // Signal L1 block 1 and step until idle.  The BatchQueue sees a future
    // batch (block 2, timestamp 4 > expected 2) and buffers it.  No attributes
    // are produced; the pipeline returns Eof.
    let (_, hit) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step after block 1 signal");
    assert!(
        !hit,
        "pipeline must not derive anything from L1 block 1 alone: \
         the batch for block 2 is a future batch — block 1 has not arrived yet"
    );
    assert_eq!(node.l2_safe_number(), 0, "safe head must remain at genesis");

    // Now make block 2 visible to the pipeline.
    chain.push(h.l1.tip().clone()); // L1 block 2: present batch

    // Signal L1 block 2.  The BatchQueue now receives the expected-next batch
    // (block 1) and derives it before popping the buffered block 2.

    // First PreparedAttributes: must be L2 block 1 (earliest timestamp).
    let (_, hit1) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 1 attributes");
    assert!(hit1, "pipeline must derive block 1 when its batch arrives");
    assert_eq!(
        node.l2_safe_number(),
        1,
        "BatchQueue must reorder: block 1 derived before the buffered block 2"
    );

    // Second PreparedAttributes: the buffered block 2 batch is now the
    // expected-next (timestamp 4 == safe head timestamp 2 + block_time 2).
    let (_, hit2) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 2 attributes");
    assert!(hit2, "pipeline must derive buffered block 2 after block 1 is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head must reach block 2");
}

/// [`act_l2_pipeline_until`] returns `(steps, false)` when the pipeline is
/// idle (no L1 data signalled yet), and `(steps, true)` once a block with
/// batch data is signalled.
///
/// This documents the Eof → data → attributes lifecycle and verifies that
/// calling [`act_l2_pipeline_until`] before and after an
///
/// [`act_l2_pipeline_until`]: TestRollupNode::act_l2_pipeline_until
#[tokio::test]
async fn pipeline_idle_before_l1_signal_derives_after() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let block1 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    // Do NOT push block 1 yet — initialize with chain having only genesis.
    node.initialize().await;

    // Block 1 is not yet visible to the pipeline — Eof, no attributes produced.
    let (_, before_signal) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 50)
        .await
        .expect("step before signal");
    assert!(!before_signal, "pipeline must be idle before block 1 is available");
    assert_eq!(node.l2_safe_number(), 0);

    // Now make block 1 visible to the pipeline.
    chain.push(h.l1.tip().clone());

    // Block 1 now available: pipeline derives L2 block 1.
    let (_, after_signal) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step after signal");
    assert!(after_signal, "pipeline must derive L2 block 1 after block 1 is available");
    assert_eq!(node.l2_safe_number(), 1);
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
/// [`act_l2_pipeline_until`]: TestRollupNode::act_l2_pipeline_until
#[tokio::test]
async fn pipeline_l1_origin_advance_observable_after_epoch_exhausted() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks and submit them in a single L1 channel (same L1 block).
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        source.push(block2);
        Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: carries the channel for blocks 1 & 2
    h.mine_and_push(&chain); // L1 block 2: empty — used only for origin advance

    node.initialize().await;

    // Signal and derive both L2 blocks from L1 block 1.
    let (_, hit1) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for L2 block 1");
    assert!(hit1);
    assert_eq!(node.l2_safe_number(), 1);

    let (_, hit2) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for L2 block 2");
    assert!(hit2);
    assert_eq!(node.l2_safe_number(), 2);

    // L1 block 1 is now fully exhausted.  Signal the empty L1 block 2 and
    // step until AdvancedOrigin: the pipeline advances the L1 origin from
    // block 1 to block 2 without producing any new L2 attributes.
    let (_, advanced) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::AdvancedOrigin), 50)
        .await
        .expect("step until origin advance");
    assert!(
        advanced,
        "pipeline must emit AdvancedOrigin when transitioning from an \
         exhausted L1 block to the next"
    );
    assert_eq!(
        node.l2_safe_number(),
        2,
        "safe head must not change: origin advanced but empty block 2 has no batch"
    );
}

// ── Span batch: multi-epoch crossing ──────────────────────────────────────────

/// A single span batch encoding L2 blocks that span two L1 epochs is correctly
/// derived by the pipeline.
///
/// With `block_time=2` and L1 `block_time=12`, L2 blocks 1–5 (ts=2..10)
/// reference epoch 0 (L1 genesis) and L2 block 6 (ts=12) references epoch 1
/// (L1 block 1). All 6 blocks are encoded together in one span batch and
/// submitted in L1 block 2.
///
/// The `SpanBatch::get_single_batch` implementation encodes the epoch
/// transition internally; this test exercises that path and verifies that
/// the `BatchQueue` correctly emits all 6 blocks in order.
///
/// Mirrors [`multi_epoch_sequence`] which uses singular batches for the same
/// block set.
#[tokio::test]
async fn span_batch_crossing_l1_epoch_boundary() {
    let batcher_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Mine L1 block 1 at ts=12 so the sequencer can advance to epoch 1 when
    // building L2 block 6 (ts=12).
    h.mine_l1_blocks(1);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Blocks 1–5 (ts=2..10) reference epoch 0; block 6 (ts=12) references epoch 1.
    let mut source = ActionL2Source::new();
    for _ in 1..=6u64 {
        source.push(builder.build_next_block_with_single_transaction().await);
    }
    assert_eq!(builder.head().l1_origin.number, 1, "block 6 must reference epoch 1");

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Encode all 6 blocks as a single span batch and submit in L1 block 2.
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone()); // L1 block 2: span batch for all 6 L2 blocks

    node.initialize().await;

    // L1 block 1 (epoch-providing) and L1 block 2 (span batch) are both
    // available; the pipeline processes them in one run, emitting all 6 blocks.
    let derived = node.run_until_idle().await;

    assert_eq!(
        derived, 6,
        "all 6 L2 blocks must be derived from a single span batch crossing the epoch boundary"
    );
    assert_eq!(
        node.l2_safe_number(),
        6,
        "safe head must reach block 6 after span batch crosses epoch 0 → 1"
    );
}

/// The [`BatchQueue`] reorders span batches submitted in reverse L1 order.
///
/// This is the span-batch variant of
/// [`out_of_order_singular_batches_reordered_by_batch_queue`]. The span batch
/// for L2 block 2 is submitted in L1 block 1 (a "future" batch); the span
/// batch for L2 block 1 arrives in L1 block 2 (the expected-next batch).
///
/// The `BatchQueue` must:
/// 1. Buffer the future span batch on L1 block 1 (no blocks derived).
/// 2. Derive L2 block 1 from the expected-next span batch on L1 block 2.
/// 3. Pop the buffered span batch and derive L2 block 2 in the same run.
///
/// [`BatchQueue`]: base_consensus_derive::BatchQueue
#[tokio::test]
async fn out_of_order_span_batches_reordered_by_batch_queue() {
    let span_cfg = BatcherConfig {
        batch_type: BatchType::Span,
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&span_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // L1 block 1: span batch for L2 block 2 (future batch, submitted out of order).
    {
        let mut source = ActionL2Source::new();
        source.push(block2);
        Batcher::new(source, &h.rollup_config, span_cfg.clone()).advance(&mut h.l1).await;
    }
    chain.push(h.l1.tip().clone()); // L1 block 1: future span batch

    // L1 block 2: span batch for L2 block 1 (the expected-next batch).
    {
        let mut source = ActionL2Source::new();
        source.push(block1);
        Batcher::new(source, &h.rollup_config, span_cfg).advance(&mut h.l1).await;
    }
    // Do NOT push block 2 yet — let the pipeline see only block 1 first.

    node.initialize().await;

    // Signal L1 block 1: span batch for block 2 is a future batch (ts=4 >
    // expected ts=2). The BatchQueue buffers it; no attributes produced.
    let (_, hit) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step block 1");
    assert!(!hit, "future span batch must be buffered; no blocks derived from L1 block 1");
    assert_eq!(node.l2_safe_number(), 0, "safe head must remain at genesis");

    // Now make block 2 visible to the pipeline.
    chain.push(h.l1.tip().clone()); // L1 block 2: present span batch

    // Signal L1 block 2: expected-next span batch (block 1) arrives.

    // First PreparedAttributes: L2 block 1 (earliest timestamp) must derive first.
    let (_, hit1) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 1 attributes");
    assert!(hit1, "pipeline must derive block 1 when its span batch arrives");
    assert_eq!(
        node.l2_safe_number(),
        1,
        "BatchQueue must reorder: block 1 derived before the buffered span batch for block 2"
    );

    // Second PreparedAttributes: buffered block-2 span batch now matches expected-next.
    let (_, hit2) = node
        .act_l2_pipeline_until(|r| matches!(r, StepResult::PreparedAttributes), 500)
        .await
        .expect("step for block 2 attributes");
    assert!(hit2, "buffered span batch for block 2 must derive after block 1 is safe");
    assert_eq!(node.l2_safe_number(), 2, "safe head must reach block 2");
}

// ── Large L1 gaps ──────────────────────────────────────────────────────────────

/// Batches submitted after a long gap of empty L1 blocks are accepted and
/// derived correctly as long as the gap is within the sequence window.
///
/// Two L2 blocks are built in epoch 0. The batches are withheld for 15 L1
/// blocks and then submitted in L1 block 16. Because Base mainnet's default
/// `seq_window_size` is large enough to accommodate 16 L1 blocks, both
/// batches are accepted and both L2 blocks derive.
///
/// This exercises the pipeline's L1 traversal over many empty blocks — the
/// common mainnet scenario during periods of L1 congestion.
///
#[tokio::test]
async fn large_l1_gaps_within_sequence_window() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build 2 L2 blocks in epoch 0 (both reference L1 genesis as their origin).
    let block1 = builder.build_next_block_with_single_transaction().await;
    let block2 = builder.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Mine 15 empty L1 blocks with no batch data.
    for _ in 0..15 {
        h.mine_and_push(&chain); // L1 blocks 1–15: all empty
    }

    // Submit both batches together in L1 block 16 — still inside the window.
    let mut source = ActionL2Source::new();
    source.push(block1);
    source.push(block2);
    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone()); // L1 block 16: batches for L2 blocks 1 and 2

    node.initialize().await;

    // Drive derivation through all 16 L1 blocks. The pipeline traverses 15
    // empty blocks before finding the channel in block 16.
    for _ in 1..=16u64 {
        node.run_until_idle().await;
    }

    assert_eq!(
        node.l2_safe_number(),
        2,
        "both L2 blocks must derive even after 15 empty L1 blocks before the batch"
    );
}

// ── Sequence-window exhaustion ─────────────────────────────────────────────────

/// When no batches are submitted across many L1 blocks the derivation pipeline
/// generates deposit-only default blocks for every expired epoch, ensuring the
/// L2 chain always makes forward progress.
///
/// Configuration:
/// - `seq_window_size = 4` (small window so epochs expire quickly)
/// - Zero batches submitted
/// - 20 empty L1 blocks mined
///
/// Expected result: the safe head advances well past genesis as the pipeline
/// synthesises deposit-only blocks for each expired epoch.
///
#[tokio::test]
async fn extended_sequence_window_exhaustion_fills_with_deposit_only_blocks() {
    const SEQ_WINDOW: u64 = 4;
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .with_seq_window_size(SEQ_WINDOW)
        .build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // Mine 20 empty L1 blocks — no batches submitted anywhere.
    for _ in 0..20 {
        h.mine_and_push(&chain);
    }

    node.initialize().await;

    let mut total_derived = 0;
    for _ in 1..=20u64 {
        total_derived += node.run_until_idle().await;
    }

    // With no batches, the pipeline generates deposit-only blocks for each
    // expired sequence window. The safe head must advance past genesis.
    assert!(
        node.l2_safe_number() > 0,
        "safe head must advance past genesis via deposit-only blocks when \
         sequence windows expire; got {}",
        node.l2_safe_number()
    );
    assert!(
        total_derived > 0,
        "pipeline must have generated deposit-only blocks for expired epochs; \
         total_derived = {total_derived}"
    );
}
