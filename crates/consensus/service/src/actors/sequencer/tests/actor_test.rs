use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionPayloadV1;
use base_alloy_rpc_types_engine::{
    OpExecutionPayload, OpExecutionPayloadEnvelope, OpPayloadAttributes,
};
use base_consensus_derive::{BuilderError, PipelineErrorKind, test_utils::TestAttributesBuilder};
use base_consensus_engine::SealTaskError;
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use jsonrpsee::core::ClientError;
use rstest::rstest;

use crate::{
    ConductorError, SealState, SealStepError, SequencerActorError, UnsafePayloadGossipClientError,
    UnsealedPayloadHandle,
    actors::{
        MockConductor, MockOriginSelector, MockSequencerEngineClient,
        MockUnsafePayloadGossipClient,
        engine::EngineClientError,
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

fn dummy_attributes_with_parent() -> AttributesWithParent {
    AttributesWithParent::new(OpPayloadAttributes::default(), L2BlockInfo::default(), None, false)
}

fn handle_with_parent_number(number: u64) -> UnsealedPayloadHandle {
    handle_with_parent(number, B256::ZERO)
}

fn handle_with_parent(number: u64, hash: B256) -> UnsealedPayloadHandle {
    let parent = L2BlockInfo {
        block_info: BlockInfo { number, hash, ..Default::default() },
        ..Default::default()
    };
    UnsealedPayloadHandle {
        payload_id: Default::default(),
        attributes_with_parent: AttributesWithParent::new(
            OpPayloadAttributes::default(),
            parent,
            None,
            false,
        ),
    }
}

fn head_at(number: u64) -> L2BlockInfo {
    head_at_with_hash(number, B256::ZERO)
}

fn head_at_with_hash(number: u64, hash: B256) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo { number, hash, ..Default::default() },
        ..Default::default()
    }
}

// --- try_seal_handle tests ---

#[tokio::test]
async fn test_try_seal_handle_current_head_equals_parent_seals() {
    // head.number == parent.number AND head.hash == parent.hash → not stale; seal proceeds.
    // Use a distinct non-zero hash so the hash equality check is actually exercised.
    let hash = B256::from([0xcc; 32]);

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(head_at_with_hash(5, hash)));
    client.expect_get_sealed_payload().times(1).return_once(|_, _| Ok(dummy_envelope()));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let (sealer, dur) = actor.try_seal_handle(handle_with_parent(5, hash)).await.unwrap().unwrap();
    assert_eq!(sealer.state, SealState::Sealed);
    assert!(dur < Duration::from_secs(10));
}

#[tokio::test]
async fn test_try_seal_handle_current_head_ahead_of_parent_discards() {
    // head > parent → stale; seal_payload must NOT be called.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(6)));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_try_seal_handle_same_height_reorg_discards() {
    // head.number == parent.number but head.hash != parent.hash → same-height reorg; discard.
    let parent_hash = B256::from([0xaa; 32]);
    let reorged_hash = B256::from([0xbb; 32]);

    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(move || Ok(head_at_with_hash(5, reorged_hash)));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent(5, parent_hash)).await;

    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_try_seal_handle_get_unsafe_head_error_propagates() {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(|| Err(EngineClientError::RequestError("channel closed".to_string())));
    client.expect_get_sealed_payload().times(0);

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_try_seal_handle_fatal_seal_error_cancels_and_propagates() {
    // A fatal seal error must cancel the token and return Err.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(5)));
    client.expect_get_sealed_payload().times(1).return_once(|_, _| {
        Err(EngineClientError::SealError(SealTaskError::DepositOnlyPayloadFailed))
    });

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.is_err());
    assert!(actor.cancellation_token.is_cancelled());
}

#[tokio::test]
async fn test_try_seal_handle_non_fatal_seal_error_returns_none() {
    // A non-fatal seal error must return Ok(None) and leave the token uncancelled.
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(head_at(5)));
    client
        .expect_get_sealed_payload()
        .times(1)
        .return_once(|_, _| Err(EngineClientError::SealError(SealTaskError::HoloceneInvalidFlush)));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = actor.try_seal_handle(handle_with_parent_number(5)).await;

    assert!(result.unwrap().is_none());
    assert!(!actor.cancellation_token.is_cancelled());
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
