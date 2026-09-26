//! Recovery of one persistent [`Batcher`] on every fork scheduled on Base mainnet: a channel
//! that timed out in derivation, a confirmed batch that derivation passed over, and a safe
//! head that went back. Each test ends with derivation reading what the same batcher resent.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_genesis::RollupConfig;

/// The derivation channel timeout, in L1 blocks.
const CHANNEL_TIMEOUT: u64 = 2;

/// Calldata frames of 80 bytes, so one block spans several transactions, and a channel that
/// closes one L1 block after it opened, below [`CHANNEL_TIMEOUT`] as
/// `EncoderConfig::validate_for_rollup_config` requires.
fn batcher_config() -> BatcherConfig {
    BatcherConfig {
        encoder: EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 80,
            max_channel_duration: 1,
            ..EncoderConfig::default()
        },
        ..BatcherConfig::default()
    }
}

/// Every fork through Cobalt active from genesis, with [`CHANNEL_TIMEOUT`].
fn rollup_config(batcher: &BatcherConfig) -> RollupConfig {
    TestRollupConfigBuilder::base_mainnet(batcher)
        .all_forks_active()
        .with_cobalt_at(0)
        .with_channel_timeout(CHANNEL_TIMEOUT)
        .build()
}

/// A channel whose first frame landed but whose window closed before the rest is dropped by
/// derivation. The batcher, seeing the same L1 blocks, replays the block in a fresh channel
/// without being told, and derivation reads it from that channel.
#[tokio::test]
async fn batcher_replays_a_channel_that_timed_out_before_its_last_frame_landed() {
    let batcher_cfg = batcher_config();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_config(&batcher_cfg));
    let mut sequencer = h.create_l2_sequencer(SharedL1Chain::from_blocks(h.l1.chain().to_vec()));
    let block = sequencer.build_next_block_with_single_transaction().await;
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let batcher =
        Batcher::new(ActionL2Source::from_blocks([block.clone()]), &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;
    let frames = batcher.pending_count();
    assert!(frames >= 2, "the block must span several frames");

    // Frame 0 lands in L1 block 1, the rest stays in the mempool.
    batcher.stage_n_frames(&mut h.l1, 1);
    h.mine_and_push(&chain);
    batcher.observe_l1_block(h.l1.tip()).await;

    // The next CHANNEL_TIMEOUT + 1 L1 blocks carry nothing: once L1 passes the block frame
    // 0 landed in plus CHANNEL_TIMEOUT, derivation has timed the channel out and the batcher
    // re-encodes the block in a fresh channel. That channel is still open, so the stale
    // frames are all the batcher has in the mempool.
    for _ in 0..=CHANNEL_TIMEOUT {
        h.mine_and_push(&chain);
        batcher.observe_l1_block(h.l1.tip()).await;
    }
    assert_eq!(batcher.pending_count(), frames - 1, "the fresh channel is still open");

    // The next L1 block closes the fresh channel, and the one after carries it, behind the
    // stale frames.
    h.mine_and_push(&chain);
    batcher.observe_l1_block(h.l1.tip()).await;
    batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    assert_eq!(node.run_until_idle().await, 1, "the replayed channel derives the block");
    assert_eq!(
        node.l2_safe().block_info.hash,
        block.header.hash_slow(),
        "derivation read the block from the fresh channel"
    );
}

/// A batch confirmed on L1 then dropped by an L1 reorg is never derived: derivation passes
/// its inclusion height with the safe head unchanged. The batcher, told so, resends the block
/// and derivation reads it on the new fork.
#[tokio::test]
async fn batcher_resends_a_confirmed_batch_that_derivation_passed_over() {
    let batcher_cfg = batcher_config();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_config(&batcher_cfg));
    let mut sequencer = h.create_l2_sequencer(SharedL1Chain::from_blocks(h.l1.chain().to_vec()));
    let block = sequencer.build_next_block_with_single_transaction().await;
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // The batch lands, then an L1 reorg replaces its block with empty ones. The batcher only
    // learns of it from derivation, which passes the inclusion height on the new fork.
    let batcher =
        Batcher::new(ActionL2Source::from_blocks([block.clone()]), &h.rollup_config, batcher_cfg);
    batcher.encode_only().await;
    let inclusion = batcher.mine_pending(&mut h.l1).await;
    h.l1.reorg_to(inclusion - 1).expect("reorg below the inclusion block");
    while h.l1.latest_number() <= inclusion {
        h.mine_and_push(&chain);
        batcher.observe_l1_block(h.l1.tip()).await;
    }
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 0, "the batch is gone from L1");
    let status = node.derivation_status();
    assert!(
        status.current_l1.is_some_and(|l1| l1.number > inclusion),
        "derivation passed the inclusion height"
    );

    batcher.observe_derivation(status).await;
    // The next L1 block closes the fresh channel, the one after carries it.
    h.mine_and_push(&chain);
    batcher.observe_l1_block(h.l1.tip()).await;
    batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    assert_eq!(node.run_until_idle().await, 1, "the resent batch derives the block");
    assert_eq!(
        node.l2_safe().block_info.hash,
        block.header.hash_slow(),
        "derivation read the block from the resent batch"
    );
}

/// When derivation loses derived blocks to an L1 reorg, its safe head goes back. The batcher,
/// told so, resends every block above the new safe head, and derivation reads them again.
#[tokio::test]
async fn batcher_resends_the_blocks_above_a_safe_head_that_went_back() {
    let batcher_cfg = batcher_config();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_config(&batcher_cfg));
    let mut sequencer = h.create_l2_sequencer(SharedL1Chain::from_blocks(h.l1.chain().to_vec()));
    let blocks = sequencer.build_next_blocks_with_single_transactions(3).await;
    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    // The three blocks derive from L1 block 1, and the batcher hears the safe head is 3.
    let batcher =
        Batcher::new(ActionL2Source::from_blocks(blocks.clone()), &h.rollup_config, batcher_cfg);
    batcher.advance(&mut h.l1).await;
    chain.push(h.l1.tip().clone());
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 3, "the three blocks derive");
    batcher.observe_derivation(node.derivation_status()).await;

    // An L1 reorg replaces L1 block 1 with an empty block. Reset the node to genesis, as its
    // L1 reorg handling would, and tell the batcher the safe head is back to 0.
    h.l1.reorg_to(0).expect("reorg to genesis");
    chain.truncate_to(0);
    h.mine_and_push(&chain);
    batcher.observe_l1_block(h.l1.tip()).await;
    node.act_reset(h.l2_genesis()).await;
    batcher.observe_derivation(node.derivation_status()).await;

    // The next L1 block closes the fresh channel, the one after carries it.
    h.mine_and_push(&chain);
    batcher.observe_l1_block(h.l1.tip()).await;
    batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    assert_eq!(node.run_until_idle().await, 3, "the resent blocks derive again");
    assert_eq!(
        node.l2_safe().block_info.hash,
        blocks[2].header.hash_slow(),
        "derivation read the three blocks again"
    );
}
