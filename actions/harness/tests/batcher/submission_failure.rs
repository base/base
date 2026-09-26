//! Batcher recovery when an L1 submission fails: the driver requeues the frame and sends it
//! again. The harness fails a send with an immediate error, without any L1 interaction.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// Failed submissions are retried until one goes through, and derivation reads the block
/// from the one that did.
#[tokio::test]
async fn failed_submissions_are_retried_until_one_lands() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let block = sequencer.build_next_block_with_single_transaction().await;

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let batcher = Batcher::new(ActionL2Source::from_blocks([block]), &h.rollup_config, batcher_cfg);
    batcher.fail_next_n_submissions(3);
    batcher.encode_only().await;
    assert_eq!(batcher.pending_count(), 1, "the frame is resubmitted after every failure");

    batcher.mine_pending(&mut h.l1).await;
    chain.push(h.l1.tip().clone());

    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 1, "the retry derives the block");
    assert_eq!(node.l2_safe_number(), 1);
}
