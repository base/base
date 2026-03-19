//! Tests for [`GetPayloadTask::execute`].

use std::sync::Arc;

use alloy_primitives::{B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadFieldV2, ExecutionPayloadV1, PayloadId,
};
use base_consensus_genesis::RollupConfig;
use rstest::rstest;
use tokio::sync::mpsc;

use crate::{
    EngineTaskExt, GetPayloadTask, SealTaskError,
    test_utils::{TestAttributesBuilder, TestEngineStateBuilder, test_engine_client_builder},
};

/// A minimal all-zeros `ExecutionPayloadEnvelopeV2` for testing.
fn v2_envelope() -> ExecutionPayloadEnvelopeV2 {
    ExecutionPayloadEnvelopeV2 {
        execution_payload: ExecutionPayloadFieldV2::V1(ExecutionPayloadV1 {
            parent_hash: B256::ZERO,
            fee_recipient: Default::default(),
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number: 0,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::ZERO,
            block_hash: B256::ZERO,
            transactions: vec![],
        }),
        block_value: U256::ZERO,
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
