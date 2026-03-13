#![doc = "Action tests for L2 unsafe-head gossip simulation."]

use std::sync::Arc;

use base_action_harness::{
    ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider, ActionL2Source,
    ActionTestHarness, BatcherConfig, L1MinerConfig, L2Verifier, SharedL1Chain, VerifierPipeline,
    block_info_from,
};
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_consensus_registry::Registry;
use base_protocol::{BlockInfo, L2BlockInfo};

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// Mirrors the helper in `derivation.rs`: starts from the Base mainnet config
/// and overrides only the fields that must differ for in-memory action tests.
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

/// Create a verifier wired to `h`'s current L1 chain, returning both the
/// verifier and the shared chain.
///
/// Extracted to avoid duplicating the 20-line constructor in every test.
fn make_verifier(
    h: &ActionTestHarness,
    rollup_cfg: &RollupConfig,
) -> (L2Verifier<VerifierPipeline>, SharedL1Chain) {
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let rollup_config = Arc::new(rollup_cfg.clone());
    let l1_chain_config = Arc::new(L1ChainConfig::default());
    let l1_provider = ActionL1ChainProvider::new(chain.clone());
    let dap_source = ActionDataSource::new(chain.clone(), rollup_cfg.batch_inbox_address);
    let genesis_l1 = block_info_from(h.l1.chain().first().expect("genesis always present"));
    let safe_head = L2BlockInfo {
        block_info: BlockInfo {
            hash: rollup_cfg.genesis.l2.hash,
            number: rollup_cfg.genesis.l2.number,
            parent_hash: Default::default(),
            timestamp: rollup_cfg.genesis.l2_time,
        },
        l1_origin: alloy_eips::BlockNumHash { number: genesis_l1.number, hash: genesis_l1.hash },
        seq_num: 0,
    };
    let l2_provider = ActionL2ChainProvider::from_genesis(rollup_cfg);
    let verifier = L2Verifier::new(
        rollup_config,
        l1_chain_config,
        l1_provider,
        dap_source,
        l2_provider,
        safe_head,
        genesis_l1,
    );
    (verifier, chain)
}

/// Simulates the full op-e2e gossip pattern:
///
/// 1. A sequencer builds 5 L2 blocks.
/// 2. Each block is gossiped to the verifier, advancing `unsafe_head` to 5.
///    Throughout this phase `safe_head` stays at genesis (0).
/// 3. All blocks are batched and submitted to L1; one L1 block is mined.
/// 4. The verifier is signalled about the new L1 head and runs full derivation.
///    `safe_head` catches up to `unsafe_head` (both at 5).
#[tokio::test]
async fn test_unsafe_chain_advances_safe_catches_up() {
    const L2_BLOCK_COUNT: u64 = 5;

    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // --- Phase 1: Build L2 blocks with the sequencer. ---
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut blocks = Vec::with_capacity(L2_BLOCK_COUNT as usize);
    for _ in 0..L2_BLOCK_COUNT {
        blocks.push(sequencer.build_next_block().expect("build L2 block"));
    }

    // --- Phase 2 (setup): Batch + mine so the verifier can later derive. ---
    // All 5 blocks are encoded into one batcher transaction and mined into
    // a single L1 block before the verifier is created.
    let mut source = ActionL2Source::new();
    for block in &blocks {
        source.push(block.clone());
    }
    let mut batcher = h.create_batcher(source, batcher_cfg);
    batcher.advance().expect("batcher should encode all blocks");
    drop(batcher);
    h.l1.mine_block();
    let l1_block_1 = block_info_from(h.l1.tip());

    // Create the verifier AFTER mining so the SharedL1Chain snapshot already
    // contains the L1 block with the batch.
    let (mut verifier, _chain) = make_verifier(&h, &rollup_cfg);

    // Initialize: seed the genesis SystemConfig and drain the empty genesis
    // L1 block so IndexedTraversal is ready for new block signals.
    verifier.initialize().await.expect("initialize should succeed");

    // --- Phase 2: Gossip each block into the verifier. ---
    for block in &blocks {
        verifier
            .act_l2_unsafe_gossip_receive(block)
            .expect("gossip receive should succeed");
    }

    assert_eq!(
        verifier.l2_unsafe().block_info.number,
        L2_BLOCK_COUNT,
        "unsafe_head should have advanced to block {L2_BLOCK_COUNT} after gossip"
    );
    assert_eq!(
        verifier.l2_safe().block_info.number,
        0,
        "safe_head should still be at genesis before derivation"
    );

    // --- Phase 3+4: Signal L1 head and run derivation. ---
    verifier.act_l1_head_signal(l1_block_1).await.expect("L1 head signal should succeed");
    let derived =
        verifier.act_l2_pipeline_full().await.expect("pipeline full should succeed");

    assert_eq!(derived, L2_BLOCK_COUNT as usize, "expected {L2_BLOCK_COUNT} L2 blocks derived");
    assert_eq!(
        verifier.l2_safe().block_info.number,
        L2_BLOCK_COUNT,
        "safe_head should have caught up to block {L2_BLOCK_COUNT}"
    );
    assert_eq!(
        verifier.l2_unsafe().block_info.number,
        verifier.l2_safe().block_info.number,
        "unsafe_head and safe_head should be equal after safe chain caught up"
    );
}

/// Verify that gossiped blocks arriving out of sequential order are silently
/// dropped and do not advance `unsafe_head`.
///
/// The guard accepts only `block.number == unsafe_head.number + 1`.  Injecting
/// block 3 when `unsafe_head` is at genesis (0) is a gap-jump: 3 ≠ 0 + 1,
/// so the block is dropped and `unsafe_head` stays at 0.
#[tokio::test]
async fn test_out_of_order_gossip_is_dropped() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg.clone());

    // Build 3 sequential L2 blocks (we will inject block 3 first, skipping 1 & 2).
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block1 = sequencer.build_next_block().expect("build block 1");
    let _block2 = sequencer.build_next_block().expect("build block 2");
    let block3 = sequencer.build_next_block().expect("build block 3");

    let (mut verifier, _chain) = make_verifier(&h, &rollup_cfg);
    verifier.initialize().await.expect("initialize should succeed");

    // Inject block 3 first — gap-jump; must be dropped.
    verifier
        .act_l2_unsafe_gossip_receive(&block3)
        .expect("out-of-order gossip must not error");
    assert_eq!(
        verifier.l2_unsafe().block_info.number,
        0,
        "unsafe_head must not advance when block 3 arrives before blocks 1 and 2"
    );

    // Inject block 1 — sequential; must advance.
    verifier
        .act_l2_unsafe_gossip_receive(&block1)
        .expect("in-order gossip must succeed");
    assert_eq!(
        verifier.l2_unsafe().block_info.number,
        1,
        "unsafe_head should advance to 1 after sequential gossip of block 1"
    );

    // Inject block 3 again — still a gap (unsafe_head=1, next expected=2); must be dropped.
    verifier
        .act_l2_unsafe_gossip_receive(&block3)
        .expect("gap gossip must not error");
    assert_eq!(
        verifier.l2_unsafe().block_info.number,
        1,
        "unsafe_head must stay at 1 when block 3 arrives before block 2"
    );
}
