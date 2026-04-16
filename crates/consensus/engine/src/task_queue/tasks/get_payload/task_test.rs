//! Tests for [`GetPayloadTask::execute`].

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV2, ExecutionPayloadEnvelopeV2, ExecutionPayloadFieldV2, ExecutionPayloadV1,
    ExecutionPayloadV2, ExecutionPayloadV3, PayloadId,
};
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelopeV5, BaseExecutionPayloadV4,
};
use base_consensus_genesis::{HardForkConfig, HardforkConfig, RollupConfig};
use rstest::rstest;
use tokio::sync::mpsc;

use crate::{
    EngineTaskExt, GetPayloadTask, SealTaskError,
    test_utils::{TestAttributesBuilder, TestEngineStateBuilder, test_engine_client_builder},
};

/// A non-zero `ExecutionPayloadEnvelopeV2` for testing.
fn v2_envelope() -> ExecutionPayloadEnvelopeV2 {
    ExecutionPayloadEnvelopeV2 {
        execution_payload: ExecutionPayloadFieldV2::V1(ExecutionPayloadV1 {
            parent_hash: B256::repeat_byte(0x11),
            fee_recipient: Address::repeat_byte(0x22),
            state_root: B256::repeat_byte(0x33),
            receipts_root: B256::repeat_byte(0x44),
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::repeat_byte(0x55),
            block_number: 1,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 100,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::from(1_000_000_000u64),
            block_hash: B256::repeat_byte(0x66),
            transactions: vec![],
        }),
        block_value: U256::from(500_000_000_000u64),
    }
}

/// A non-zero [`BaseExecutionPayloadEnvelopeV5`] for Osaka / Base Azul testing.
fn v5_envelope() -> BaseExecutionPayloadEnvelopeV5 {
    BaseExecutionPayloadEnvelopeV5 {
        execution_payload: BaseExecutionPayloadV4 {
            payload_inner: ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: B256::repeat_byte(0xAA),
                        fee_recipient: Address::repeat_byte(0xBB),
                        state_root: B256::repeat_byte(0xCC),
                        receipts_root: B256::repeat_byte(0xDD),
                        logs_bloom: Bloom::ZERO,
                        prev_randao: B256::repeat_byte(0xEE),
                        block_number: 42,
                        gas_limit: 30_000_000,
                        gas_used: 100_000,
                        timestamp: 2000,
                        extra_data: Bytes::new(),
                        base_fee_per_gas: U256::from(7_000_000_000u64),
                        block_hash: B256::repeat_byte(0xFF),
                        transactions: vec![],
                    },
                    withdrawals: vec![],
                },
                blob_gas_used: 0,
                excess_blob_gas: 0,
            },
            withdrawals_root: B256::repeat_byte(0x77),
        },
        block_value: U256::from(1_000_000_000_000u64),
        blobs_bundle: BlobsBundleV2 { commitments: vec![], proofs: vec![], blobs: vec![] },
        should_override_builder: false,
        execution_requests: vec![],
    }
}

/// When the engine's unsafe head does not match the attributes parent, `GetPayloadTask` must
/// short-circuit and return [`SealTaskError::UnsafeHeadChangedSinceBuild`] without touching the
/// engine API.
#[tokio::test]
async fn test_parent_mismatch_returns_unsafe_head_changed_error() {
    let attributes = TestAttributesBuilder::new().build();

    // Build engine state whose unsafe head hash/number differ from the attributes parent.
    // test_block_info(2) produces block number 2 while the default attributes parent is block 0.
    let client = test_engine_client_builder().build();
    let mismatched_unsafe_head = crate::test_utils::test_block_info(2);
    let mut state = TestEngineStateBuilder::new().with_unsafe_head(mismatched_unsafe_head).build();

    let task = GetPayloadTask::new(
        Arc::new(client),
        Arc::new(RollupConfig::default()),
        PayloadId::default(),
        attributes,
        None,
    );

    let result = task.execute(&mut state).await;

    assert!(
        matches!(result, Err(SealTaskError::UnsafeHeadChangedSinceBuild)),
        "expected UnsafeHeadChangedSinceBuild, got {result:?}"
    );
}

/// When the unsafe head matches the attributes parent and the engine returns a valid payload,
/// `GetPayloadTask` must succeed and deliver the envelope — either via the result channel
/// (when one is provided) or as the direct task return value.
#[rstest]
#[tokio::test]
async fn test_get_payload_v2_success(#[values(true, false)] with_channel: bool) {
    let attributes = TestAttributesBuilder::new().build();
    let parent = attributes.parent;

    // RollupConfig::default() has no ecotone_time set → get_payload_v2 is selected.
    let client = test_engine_client_builder().with_execution_payload_v2(v2_envelope()).build();

    let mut state = TestEngineStateBuilder::new().with_unsafe_head(parent).build();

    let (tx, mut rx) = mpsc::channel(1);
    let task = GetPayloadTask::new(
        Arc::new(client),
        Arc::new(RollupConfig::default()),
        PayloadId::default(),
        attributes,
        if with_channel { Some(tx) } else { None },
    );

    let result = task.execute(&mut state).await;

    assert!(result.is_ok(), "task should succeed, got {result:?}");

    if with_channel {
        let channel_result = rx.recv().await.expect("channel should have a result");
        assert!(channel_result.is_ok(), "channel result should be Ok, got {channel_result:?}");
    }
}

/// When the unsafe head matches the attributes parent and the engine returns a valid V5 payload
/// (Osaka / Base Azul), `GetPayloadTask` must call `get_payload_v5`, wrap the inner
/// [`BaseExecutionPayloadV4`] as an [`BaseExecutionPayload::V4`] variant, and source
/// `parent_beacon_block_root` from the attributes rather than the payload envelope.
#[rstest]
#[tokio::test]
async fn test_get_payload_v5_success(#[values(true, false)] with_channel: bool) {
    let attributes = TestAttributesBuilder::new().build();
    let parent = attributes.parent;

    // Activate Base Azul (Osaka) at the default attributes timestamp (2000) so that
    // `EngineGetPayloadVersion::V5` is selected.
    let cfg = Arc::new(RollupConfig {
        hardforks: HardForkConfig {
            base: HardforkConfig { azul: Some(2000) },
            ..Default::default()
        },
        ..Default::default()
    });

    let client = test_engine_client_builder().with_execution_payload_v5(v5_envelope()).build();
    let mut state = TestEngineStateBuilder::new().with_unsafe_head(parent).build();

    let (tx, mut rx) = mpsc::channel(1);
    let task = GetPayloadTask::new(
        Arc::new(client),
        cfg,
        PayloadId::default(),
        attributes,
        if with_channel { Some(tx) } else { None },
    );

    let result = task.execute(&mut state).await;
    assert!(result.is_ok(), "task should succeed, got {result:?}");

    if with_channel {
        let channel_result = rx.recv().await.expect("channel should have a result");
        assert!(channel_result.is_ok(), "channel result should be Ok, got {channel_result:?}");
        let envelope = channel_result.unwrap();
        // V5 wraps the execution payload as the V4 variant inside BaseExecutionPayload.
        assert!(
            matches!(envelope.execution_payload, BaseExecutionPayload::V4(_)),
            "V5 get_payload should produce a V4 execution payload variant, got {:?}",
            envelope.execution_payload
        );
        // V5 omits parent_beacon_block_root from the response envelope; the task sources
        // it from the attributes. TestAttributesBuilder::new() defaults to Some(B256::ZERO).
        assert_eq!(
            envelope.parent_beacon_block_root,
            Some(B256::ZERO),
            "parent_beacon_block_root should be sourced from attributes for V5 payloads"
        );
    }
}

/// When the engine returns an error (no payload configured in the mock), `GetPayloadTask` must
/// surface the error — either by sending it via the result channel or by returning it from
/// `execute` when no channel is provided.
#[rstest]
#[tokio::test]
async fn test_get_payload_failure_propagates(#[values(true, false)] with_channel: bool) {
    let attributes = TestAttributesBuilder::new().build();
    let parent = attributes.parent;

    // No payload configured → mock returns a transport error.
    let client = test_engine_client_builder().build();
    let mut state = TestEngineStateBuilder::new().with_unsafe_head(parent).build();

    let (tx, mut rx) = mpsc::channel(1);
    let task = GetPayloadTask::new(
        Arc::new(client),
        Arc::new(RollupConfig::default()),
        PayloadId::default(),
        attributes,
        if with_channel { Some(tx) } else { None },
    );

    let result = task.execute(&mut state).await;

    if with_channel {
        // With a channel the task itself returns Ok(()); the error goes into the channel.
        assert!(result.is_ok(), "task should return Ok when a channel absorbs the error");
        let channel_result = rx.recv().await.expect("channel should have a result");
        assert!(
            matches!(channel_result, Err(SealTaskError::GetPayloadFailed(_))),
            "channel should contain GetPayloadFailed, got {channel_result:?}"
        );
    } else {
        // Without a channel the task propagates the error directly.
        assert!(
            matches!(result, Err(SealTaskError::GetPayloadFailed(_))),
            "expected GetPayloadFailed, got {result:?}"
        );
    }
}
