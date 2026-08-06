//! Action tests for shadow-sequencer canonical catch-up.

use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, ExecutionPayloadConverter,
    L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_consensus_node::CanonicalUnsafeCatchup;

/// A late sequencer retains future unsafe gossip while deriving safe blocks, then applies the
/// contiguous unsafe suffix in canonical order.
#[tokio::test]
async fn late_shadow_catches_up_safe_then_unsafe() {
    const SAFE_BLOCKS: usize = 5;
    const UNSAFE_BLOCKS: usize = 3;

    let batcher_config = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };
    let rollup_config = TestRollupConfigBuilder::base_mainnet(&batcher_config).build();
    let mut harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_config);
    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let mut sequencer = harness.create_l2_sequencer(l1_chain);
    let blocks = sequencer
        .build_next_blocks_with_single_transactions((SAFE_BLOCKS + UNSAFE_BLOCKS) as u64)
        .await;

    let (mut shadow, chain) = harness.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
    );
    let mut source = ActionL2Source::new();
    for block in &blocks[..SAFE_BLOCKS] {
        source.push(block.clone());
    }
    Batcher::new(source, &harness.rollup_config, batcher_config).advance(&mut harness.l1).await;
    chain.push(harness.l1.tip().clone());
    shadow.initialize().await;

    let envelope = |index: usize| {
        let block = &blocks[index];
        let hash = block.header.hash_slow();
        let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(hash, block);
        BaseExecutionPayloadEnvelope {
            execution_payload,
            parent_beacon_block_root: block.header.parent_beacon_block_root,
        }
    };
    let mut catchup = CanonicalUnsafeCatchup::default();
    catchup.buffer_payload(envelope(7));
    catchup.buffer_payload(envelope(5));
    catchup.buffer_payload(envelope(6));

    assert_eq!(shadow.l2_unsafe_number(), 0, "future gossip must remain outside the engine queue");
    assert_eq!(shadow.run_until_idle().await, SAFE_BLOCKS);
    assert_eq!(shadow.l2_safe_number(), SAFE_BLOCKS as u64);

    let payloads = catchup.contiguous_payloads(shadow.l2_unsafe());
    assert_eq!(payloads.len(), UNSAFE_BLOCKS);
    for payload in payloads {
        let block = ExecutionPayloadConverter::block_from_envelope(&payload)
            .expect("canonical payload must convert back into a block");
        shadow.act_l2_unsafe_gossip_receive(&block);
        catchup.commit(shadow.l2_unsafe());
    }

    assert_eq!(shadow.l2_safe_number(), SAFE_BLOCKS as u64);
    assert_eq!(shadow.l2_unsafe_number(), (SAFE_BLOCKS + UNSAFE_BLOCKS) as u64);
    assert!(catchup.is_complete(shadow.l2_unsafe(), shadow.l2_safe()));
}
