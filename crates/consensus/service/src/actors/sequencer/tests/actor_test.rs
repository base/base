use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionPayloadV1;
use base_alloy_rpc_types_engine::{
    OpExecutionPayload, OpExecutionPayloadEnvelope, OpPayloadAttributes,
};
use base_consensus_derive::{BuilderError, PipelineErrorKind, test_utils::TestAttributesBuilder};
use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use jsonrpsee::core::ClientError;
use rstest::rstest;

#[cfg(test)]
use crate::{
    ConductorError, SealState, SealStepError, SequencerActorError, UnsealedPayloadHandle,
    actors::{
        MockConductor, MockOriginSelector, MockSequencerEngineClient,
        MockUnsafePayloadGossipClient,
        sequencer::{PayloadSealer, tests::test_util::test_actor},
    },
};

fn dummy_envelope() -> OpExecutionPayloadEnvelope {
    OpExecutionPayloadEnvelope {
        parent_beacon_block_root: None,
        execution_payload: OpExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash: B256::ZERO,
            fee_recipient: alloy_primitives::Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: alloy_primitives::Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number: 1,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: alloy_primitives::Bytes::new(),
            base_fee_per_gas: alloy_primitives::U256::ZERO,
            block_hash: B256::ZERO,
            transactions: vec![],
        }),
    }
}

fn conductor_rpc_error() -> ConductorError {
    ConductorError::Rpc(ClientError::Custom("test conductor error".to_string()))
}

fn dummy_attributes_with_parent() -> OpAttributesWithParent {
    OpAttributesWithParent::new(OpPayloadAttributes::default(), L2BlockInfo::default(), None, false)
}

// --- build tests ---

#[rstest]
#[case::temp(PipelineErrorKind::Temporary(BuilderError::Custom(String::new()).into()), false)]
#[case::reset(PipelineErrorKind::Reset(BuilderError::Custom(String::new()).into()), false)]
#[case::critical(PipelineErrorKind::Critical(BuilderError::Custom(String::new()).into()), true)]
#[tokio::test]
async fn test_build_unsealed_payload_prepare_payload_attributes_error(
    #[case] forced_error: PipelineErrorKind,
    #[case] expect_err: bool,
) {
    let mut client = MockSequencerEngineClient::new();

    let unsafe_head = L2BlockInfo::default();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(unsafe_head));
    client.expect_start_build_block().times(0);
    // Reset pipeline errors no longer trigger engine reset — the attributes builder is stateless
    // so resetting the engine would only rewind the unsafe head without aiding recovery.
    client.expect_reset_engine_forkchoice().times(0);

    let l1_origin = BlockInfo::default();
    let mut origin_selector = MockOriginSelector::new();
    origin_selector.expect_next_l1_origin().times(1).return_once(move |_, _| Ok(l1_origin));

    let attributes_builder = TestAttributesBuilder { attributes: vec![Err(forced_error)] };

    let mut actor = test_actor();
    actor.builder.origin_selector = origin_selector;
    actor.builder.engine_client = Arc::new(client);
    actor.builder.attributes_builder = attributes_builder;

    let result = actor.builder.build().await;
    if expect_err {
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SequencerActorError::AttributesBuilder(PipelineErrorKind::Critical(_))
        ));
    } else {
        assert!(result.is_ok());
    }
}

// --- seal_payload tests ---

#[tokio::test]
async fn test_seal_payload_success_returns_sealer() {
    let envelope = dummy_envelope();

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_sealed_payload().times(1).return_once(move |_, _| Ok(envelope));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let handle = UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: dummy_attributes_with_parent(),
    };
    let sealer = actor.seal_payload(&handle).await;

    assert!(sealer.is_ok());
    assert_eq!(sealer.unwrap().state, SealState::Sealed);
}

#[tokio::test]
async fn test_seal_payload_failure_propagates() {
    use crate::actors::engine::EngineClientError;

    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_sealed_payload()
        .times(1)
        .return_once(|_, _| Err(EngineClientError::RequestError("engine offline".to_string())));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let handle = UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: dummy_attributes_with_parent(),
    };
    let result = actor.seal_payload(&handle).await;

    assert!(result.is_err());
}

// --- PayloadSealer::step tests ---

#[tokio::test]
async fn test_sealer_full_pipeline_no_conductor() {
    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine.expect_insert_unsafe_payload().times(1).return_once(|_| Ok(()));

    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    assert_eq!(sealer.state, SealState::Sealed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(!result.unwrap());
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(!result.unwrap());
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.unwrap());
}

#[tokio::test]
async fn test_sealer_full_pipeline_with_conductor() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(1).return_once(|_| Ok(()));

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine.expect_insert_unsafe_payload().times(1).return_once(|_| Ok(()));

    let conductor = Some(conductor);
    let mut sealer = PayloadSealer::new(envelope);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(!result.unwrap());
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(!result.unwrap());
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.unwrap());
}

#[tokio::test]
async fn test_sealer_conductor_failure_stays_sealed() {
    let envelope = dummy_envelope();

    let mut conductor = MockConductor::new();
    conductor.expect_commit_unsafe_payload().times(1).return_once(|_| Err(conductor_rpc_error()));

    let gossip = MockUnsafePayloadGossipClient::new();
    let engine = MockSequencerEngineClient::new();

    let conductor = Some(conductor);
    let mut sealer = PayloadSealer::new(envelope);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Conductor(_)));
    assert_eq!(sealer.state, SealState::Sealed);
}

#[tokio::test]
async fn test_sealer_gossip_failure_stays_committed() {
    use crate::UnsafePayloadGossipClientError;

    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| {
        Err(UnsafePayloadGossipClientError::RequestError("channel closed".to_string()))
    });

    let engine = MockSequencerEngineClient::new();
    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    assert_eq!(sealer.state, SealState::Committed);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Gossip(_)));
    assert_eq!(sealer.state, SealState::Committed);
}

#[tokio::test]
async fn test_sealer_insert_failure_stays_gossiped() {
    use crate::actors::engine::EngineClientError;

    let envelope = dummy_envelope();

    let mut gossip = MockUnsafePayloadGossipClient::new();
    gossip.expect_schedule_execution_payload_gossip().times(1).return_once(|_| Ok(()));

    let mut engine = MockSequencerEngineClient::new();
    engine
        .expect_insert_unsafe_payload()
        .times(1)
        .return_once(|_| Err(EngineClientError::RequestError("channel closed".to_string())));

    let conductor: Option<MockConductor> = None;
    let mut sealer = PayloadSealer::new(envelope);

    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    let _ = sealer.step(&conductor, &gossip, &engine).await.unwrap();
    assert_eq!(sealer.state, SealState::Gossiped);

    let result = sealer.step(&conductor, &gossip, &engine).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SealStepError::Insert(_)));
    assert_eq!(sealer.state, SealState::Gossiped);
}
