//! Tests for `ConsolidateTask::execute`

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{FixedBytes, b256};
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::Block as RpcBlock;
use base_common_rpc_types::Transaction as OpTransaction;
use base_consensus_genesis::RollupConfig;

use crate::{
    ConsolidateTask, EngineTaskExt,
    task_queue::tasks::consolidate::task::ConsolidateInput,
    test_utils::{TestAttributesBuilder, TestEngineStateBuilder, test_engine_client_builder},
};

/// Verifies that consolidation does NOT fatally error when safe head is behind
/// the unsafe head and the derived attributes don't match the existing block.
///
/// Previously, `SealTask` compared `state.sync_state.unsafe_head()` (the chain
/// tip, e.g. block 76) against `attributes.parent` (the safe head, e.g. block 34)
/// and returned `UnsafeHeadChangedSinceBuild` with Critical severity, crashing the
/// engine.  Op-node has no such check — the `BuildTask` already FCU'd the EL to the
/// correct parent, so the comparison is invalid.
///
/// After the fix the reconcile path proceeds to `seal_and_canonicalize_block`
/// directly, matching the reference node's behaviour.
///
/// This test FAILS on unfixed main and PASSES after the fix lands.
#[tokio::test]
async fn consolidate_does_not_crash_when_safe_behind_unsafe_and_attributes_mismatch() {
    let safe_head = crate::test_utils::test_block_info(34);
    let unsafe_head = crate::test_utils::test_block_info(76);

    // Attributes produced by derivation: parent = safe_head (block 34) → block 35.
    let attributes =
        TestAttributesBuilder::new().with_parent(safe_head).with_timestamp(2000).build();

    // Engine state: safe at 34, unsafe at 76.
    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(safe_head)
        .with_finalized_head(safe_head)
        .build();

    // Build a block at height 35 that does NOT match the attributes.
    // The key mismatch: parent_hash differs from attributes.parent.block_info.hash.
    // This makes `is_consistent_with_block` return false → triggers reconcile path.
    let mut mismatched_block = RpcBlock::<OpTransaction>::default();
    mismatched_block.header.inner.number = 35;
    mismatched_block.header.inner.timestamp = 2000;
    mismatched_block.header.inner.parent_hash =
        b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    // Mock client: return the mismatched block at number 35, and a Valid FCU
    // with a payload_id (needed by BuildTask inside the reconcile path).
    let valid_fcu = ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(FixedBytes([2u8; 32])),
        },
        payload_id: Some(PayloadId::new([1u8; 8])),
    };
    let client = Arc::new(
        test_engine_client_builder()
            .with_l2_block_by_label(BlockNumberOrTag::Number(35), mismatched_block)
            .with_fork_choice_updated_v2_response(valid_fcu.clone())
            .with_fork_choice_updated_v3_response(valid_fcu)
            .build(),
    );

    let task = ConsolidateTask::new(
        client,
        Arc::new(RollupConfig::default()),
        ConsolidateInput::from(attributes),
    );

    // Execute — previously this returned Critical UnsafeHeadChangedSinceBuild.
    // Now it proceeds to seal_and_canonicalize_block (which will fail for other
    // reasons in a mock environment, but crucially NOT with the stale-unsafe-head
    // check that caused the crash loop).
    let result = task.execute(&mut state).await;

    // The task may still error (e.g. GetPayload fails in the mock) but it must
    // NOT be the stale-unsafe-head error that caused the crash loop.
    // The Display string for SealTaskError::UnsafeHeadChangedSinceBuild is
    // "Unsafe head changed between build and seal".
    if let Err(ref err) = result {
        let err_msg = format!("{err}");
        assert!(
            !err_msg.contains("Unsafe head changed between build and seal"),
            "must not fail with UnsafeHeadChangedSinceBuild: {err}"
        );
    }
}
