#![doc = "Action tests for blob DA submission and mixed calldata/blob derivation."]

use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, block_info_from,
};
use base_batcher_encoder::EncoderConfig;
use base_consensus_genesis::RollupConfig;
use base_consensus_registry::Registry;

/// Build a [`RollupConfig`] wired to the given [`BatcherConfig`].
///
/// Mirrors the helper from `derivation.rs`: overrides the batcher/inbox
/// addresses and zeroes genesis timestamps so the in-memory L1 miner (which
/// starts at ts=0) is the chain origin.  Canyon through Fjord are activated at
/// genesis (timestamp 0) so brotli compression and span-batch encoding are
/// accepted throughout the test.
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

// ---------------------------------------------------------------------------
// Blob DA end-to-end
// ---------------------------------------------------------------------------

/// Encode 3 L2 blocks as EIP-4844 blobs (one blob per L2 block, each in its
/// own L1 block) and verify that the blob verifier pipeline derives all three.
///
/// Follows the same one-block-per-L1-block pattern as the calldata derivation
/// tests.  Block hashes from the sequencer are registered with the verifier so
/// that parent-hash validation succeeds for blocks 2 and 3.
#[tokio::test]
async fn batcher_blob_da_end_to_end() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut block_hashes: Vec<(u64, B256)> = Vec::new();

    // Build and submit each L2 block in its own L1 blob block.
    for i in 1..=3u64 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        let frames = batcher.encode_frames().expect("encode frames");
        batcher.submit_blob_frames(&frames);
        drop(batcher);

        h.l1.mine_block();
    }

    // Create the blob verifier AFTER mining so the SharedL1Chain snapshot
    // includes all L1 blocks with their blob sidecars.
    let (mut verifier, _chain) = h.create_blob_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    // Drive derivation one L1 block at a time.
    for i in 1..=3u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal L1 block");
        let derived = verifier.act_l2_pipeline_full().await.expect("derive pipeline");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block via blob DA");
    }

    assert_eq!(verifier.l2_safe().block_info.number, 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Multi-blob packing (many frames → many blob sidecars in one L1 block)
// ---------------------------------------------------------------------------

/// Force channel fragmentation via a tiny `max_frame_size`, then submit all
/// resulting frames as separate blob sidecars in a single L1 block.
///
/// The blob verifier must read every blob sidecar from that L1 block, reassemble
/// the channel fragments, and derive the encoded L2 block.  This exercises the
/// path where a single L1 block carries multiple blob sidecars from the same
/// channel.
///
/// An empty L1 block 2 is mined after the blob block so the `BatchQueue` has
/// a next epoch boundary available when it emits the derived L2 block.
#[tokio::test]
async fn batcher_multi_blob_packing() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 1 L2 block and encode it into multiple frames (tiny max_frame_size).
    let block = sequencer.build_next_block().expect("build L2 block");
    let hash1 = sequencer.head().block_info.hash;

    let mut source = ActionL2Source::new();
    source.push(block);
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().expect("encode frames");
    assert!(
        frames.len() >= 2,
        "expected multiple frames with max_frame_size=80, got {}",
        frames.len()
    );
    // All frames go into the same L1 block as separate blob sidecars.
    batcher.submit_blob_frames(&frames);
    drop(batcher);

    h.l1.mine_block(); // L1 block 1: all blob sidecars for the fragmented channel

    // Create verifier AFTER mining so the snapshot includes the blob block.
    let (mut verifier, _chain) = h.create_blob_verifier();
    verifier.register_block_hash(1, hash1);
    verifier.initialize().await.expect("initialize");

    // Signal L1 block 1: the pipeline reads all blob sidecars, reassembles the
    // fragmented channel, and emits the batch.  L1 block 1 is the "next epoch"
    // after epoch 0, so BatchQueue can emit as soon as it has all frames.
    let l1_block_1 = block_info_from(h.l1.block_by_number(1).expect("L1 block 1"));
    verifier.act_l1_head_signal(l1_block_1).await.expect("signal L1 block 1");
    let derived = verifier.act_l2_pipeline_full().await.expect("step after L1 block 1");

    assert_eq!(derived, 1, "expected 1 L2 block derived from multi-blob channel");
    assert_eq!(verifier.l2_safe().block_info.number, 1, "safe head should reach L2 block 1");
}

// ---------------------------------------------------------------------------
// Calldata DA (explicit)
// ---------------------------------------------------------------------------

/// Encode 3 L2 blocks as calldata frames (one calldata tx per L2 block, each
/// in its own L1 block) and verify that the calldata verifier pipeline derives
/// all three.
///
/// This mirrors `batcher_blob_da_end_to_end` but uses the calldata DA path,
/// making both paths explicit and comparable in the test suite.
#[tokio::test]
async fn batcher_calldata_da() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut block_hashes: Vec<(u64, B256)> = Vec::new();

    for i in 1..=3u64 {
        let block = sequencer.build_next_block().expect("build L2 block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        let frames = batcher.encode_frames().expect("encode frames");
        batcher.submit_frames(&frames);
        drop(batcher);

        h.l1.mine_block();
    }

    let (mut verifier, _chain) = h.create_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    for i in 1..=3u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal L1 block");
        let derived = verifier.act_l2_pipeline_full().await.expect("derive pipeline");
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block via calldata DA");
    }

    assert_eq!(verifier.l2_safe().block_info.number, 3, "safe head should reach L2 block 3");
}

// ---------------------------------------------------------------------------
// Mixed calldata + blob derivation
// ---------------------------------------------------------------------------

/// Submit 3 L2 blocks as calldata and 3 more as blobs, each in separate L1
/// blocks, then derive all 6 using the blob verifier pipeline.
///
/// The blob verifier pipeline reads both calldata (`batcher_txs`) and blob
/// sidecars from each L1 block, making it suitable for mixed-DA sequences
/// without any pipeline configuration changes.
///
/// This exercises the DA-switching scenario: the pipeline must process calldata
/// from L1 blocks 1-3 and blob data from L1 blocks 4-6 to produce all 6 L2
/// blocks in order.
#[tokio::test]
async fn batcher_da_switching() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = rollup_config_for(&batcher_cfg);
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // One sequencer for all 6 blocks ensures a contiguous parent-hash chain.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut block_hashes: Vec<(u64, B256)> = Vec::new();

    // Blocks 1-3: submit as calldata (one block per L1 block).
    for i in 1..=3u64 {
        let block = sequencer.build_next_block().expect("build calldata block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        let frames = batcher.encode_frames().expect("encode calldata frames");
        batcher.submit_frames(&frames);
        drop(batcher);
        h.l1.mine_block();
    }

    // Blocks 4-6: submit as blobs (one block per L1 block).
    for i in 4..=6u64 {
        let block = sequencer.build_next_block().expect("build blob block");
        let hash = sequencer.head().block_info.hash;
        block_hashes.push((i, hash));

        let mut source = ActionL2Source::new();
        source.push(block);
        let mut batcher = h.create_batcher(source, batcher_cfg.clone());
        let frames = batcher.encode_frames().expect("encode blob frames");
        batcher.submit_blob_frames(&frames);
        drop(batcher);
        h.l1.mine_block();
    }

    // The blob verifier handles both calldata (batcher_txs) and blobs
    // (blob_sidecars) from each block, making it capable of deriving the
    // full mixed sequence without any pipeline changes.
    let (mut verifier, _chain) = h.create_blob_verifier();
    for (number, hash) in &block_hashes {
        verifier.register_block_hash(*number, *hash);
    }
    verifier.initialize().await.expect("initialize");

    let mut total_derived = 0;
    for i in 1..=6u64 {
        let l1_block = block_info_from(h.l1.block_by_number(i).expect("block exists"));
        verifier.act_l1_head_signal(l1_block).await.expect("signal L1 block");
        total_derived += verifier.act_l2_pipeline_full().await.expect("derive pipeline");
    }

    assert_eq!(total_derived, 6, "expected 6 L2 blocks derived (3 calldata + 3 blob)");
    assert_eq!(verifier.l2_safe().block_info.number, 6, "safe head should reach L2 block 6");
}
