//! Tests for [`SynchronizeTask::execute`].

use std::sync::Arc;

use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_common_genesis::RollupConfig;

use crate::{
    EngineTaskExt, SynchronizeTask,
    state::EngineSyncStateUpdate,
    test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder},
};

fn syncing_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: None,
        },
        payload_id: None,
    }
}

fn valid_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None },
        payload_id: None,
    }
}

#[tokio::test]
async fn valid_response_advances_sync_state() {
    let head = test_block_info(100);
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new().build();

    let task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() },
    );

    task.execute(&mut state).await.expect("should succeed");

    assert_eq!(
        state.sync_state.unsafe_head().block_info.number,
        100,
        "unsafe_head must advance on Valid response"
    );
    assert!(state.el_sync_finished, "el_sync_finished must be true after Valid");
}

#[tokio::test]
async fn syncing_response_does_not_advance_sync_state() {
    let head = test_block_info(100);
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new().with_el_sync_finished(false).build();
    let original_unsafe = state.sync_state.unsafe_head();

    let task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() },
    );

    task.execute(&mut state).await.expect("should succeed");

    assert_eq!(
        state.sync_state.unsafe_head(),
        original_unsafe,
        "unsafe_head must NOT advance on Syncing response"
    );
    assert!(!state.el_sync_finished, "el_sync_finished must remain false after Syncing");
}

#[tokio::test]
async fn syncing_then_valid_advances_state_on_second_call() {
    let head_a = test_block_info(100);
    let head_b = test_block_info(101);
    let cfg = Arc::new(RollupConfig::default());

    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new().with_el_sync_finished(false).build();

    // First call: EL returns Syncing → state stays put.
    let task = SynchronizeTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        EngineSyncStateUpdate { unsafe_head: Some(head_a), ..Default::default() },
    );
    task.execute(&mut state).await.expect("should succeed");
    assert_eq!(state.sync_state.unsafe_head().block_info.number, 0);
    assert!(!state.el_sync_finished);

    // Reconfigure mock to return Valid.
    client.set_fork_choice_updated_v3_response(valid_fcu()).await;

    // Second call: EL returns Valid → state advances.
    let task = SynchronizeTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        EngineSyncStateUpdate { unsafe_head: Some(head_b), ..Default::default() },
    );
    task.execute(&mut state).await.expect("should succeed");
    assert_eq!(
        state.sync_state.unsafe_head().block_info.number,
        101,
        "unsafe_head must advance after Valid"
    );
    assert!(state.el_sync_finished);
}
