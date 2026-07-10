//! Tests for [`SynchronizeTask::execute`].

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_common_genesis::RollupConfig;
use rstest::rstest;

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

fn invalid_fcu() -> ForkchoiceUpdated {
    ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Invalid { validation_error: "test-invalid".into() },
            latest_valid_hash: Some(B256::ZERO),
        },
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
async fn syncing_response_preserves_safe_head_when_it_is_behind_unsafe() {
    let unsafe_head = test_block_info(100);
    let safe_head = test_block_info(90);
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(test_block_info(89))
        .with_el_sync_finished(true)
        .build();
    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
        local_safe_head: Some(test_block_info(89)),
        ..Default::default()
    });

    let task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate {
            local_safe_head: Some(safe_head),
            safe_head: Some(safe_head),
            ..Default::default()
        },
    );

    task.execute(&mut state).await.expect("should succeed");

    assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    assert_eq!(state.sync_state.local_safe_head(), safe_head);
    assert_eq!(state.sync_state.safe_head(), safe_head);
    assert!(state.el_sync_finished, "el_sync_finished should remain sticky after Syncing");
}

#[tokio::test]
async fn syncing_response_does_not_preserve_safe_head_before_el_sync_finishes() {
    let unsafe_head = test_block_info(100);
    let safe_head = test_block_info(90);
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(test_block_info(89))
        .with_el_sync_finished(false)
        .build();
    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
        local_safe_head: Some(test_block_info(89)),
        ..Default::default()
    });

    let task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate {
            local_safe_head: Some(safe_head),
            safe_head: Some(safe_head),
            ..Default::default()
        },
    );

    task.execute(&mut state).await.expect("should succeed");

    assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    assert_eq!(state.sync_state.local_safe_head().block_info.number, 89);
    assert_eq!(state.sync_state.safe_head().block_info.number, 89);
    assert!(!state.el_sync_finished);
}

#[tokio::test]
async fn syncing_response_does_not_advance_safe_head_past_unsafe() {
    let unsafe_head = test_block_info(100);
    let safe_head = test_block_info(101);
    let mut preserved_finalized_head = test_block_info(100);
    preserved_finalized_head.block_info.hash = unsafe_head.block_info.hash;
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(test_block_info(95))
        .with_finalized_head(test_block_info(90))
        .with_el_sync_finished(true)
        .build();
    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
        local_safe_head: Some(unsafe_head),
        ..Default::default()
    });

    let task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate {
            local_safe_head: Some(safe_head),
            safe_head: Some(safe_head),
            finalized_head: Some(preserved_finalized_head),
            ..Default::default()
        },
    );

    task.execute(&mut state).await.expect("should succeed");

    assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    assert_eq!(state.sync_state.local_safe_head(), unsafe_head);
    assert_eq!(state.sync_state.safe_head().block_info.number, 95);
    assert_eq!(state.sync_state.finalized_head(), preserved_finalized_head);
}

#[tokio::test]
async fn syncing_response_preserves_equal_height_safe_head_only_on_same_hash() {
    let unsafe_head = test_block_info(100);
    let matching_safe_head = unsafe_head;
    let mismatched_safe_head = test_block_info(100);
    let cfg = Arc::new(RollupConfig::default());
    let client = Arc::new(
        test_engine_client_builder().with_fork_choice_updated_v3_response(syncing_fcu()).build(),
    );

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(test_block_info(99))
        .with_el_sync_finished(true)
        .build();
    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
        local_safe_head: Some(test_block_info(99)),
        ..Default::default()
    });

    let matching_task = SynchronizeTask::new(
        Arc::clone(&client),
        Arc::clone(&cfg),
        EngineSyncStateUpdate {
            local_safe_head: Some(matching_safe_head),
            safe_head: Some(matching_safe_head),
            ..Default::default()
        },
    );
    matching_task.execute(&mut state).await.expect("should succeed");
    assert_eq!(state.sync_state.safe_head(), matching_safe_head);

    let mut state = TestEngineStateBuilder::new()
        .with_unsafe_head(unsafe_head)
        .with_safe_head(test_block_info(99))
        .with_el_sync_finished(true)
        .build();
    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
        local_safe_head: Some(test_block_info(99)),
        ..Default::default()
    });
    let mismatched_task = SynchronizeTask::new(
        client,
        cfg,
        EngineSyncStateUpdate {
            local_safe_head: Some(mismatched_safe_head),
            safe_head: Some(mismatched_safe_head),
            ..Default::default()
        },
    );
    mismatched_task.execute(&mut state).await.expect("should succeed");
    assert_eq!(state.sync_state.local_safe_head().block_info.number, 99);
    assert_eq!(state.sync_state.safe_head().block_info.number, 99);
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

mod tests {
    use super::*;

    mod forkchoice_update_commit_policy {
    //! Design decision: `MockEngineClient` is sufficient here because we only need one
    //! scripted `fork_choice_updated_v3` response per execution and no call-sequence
    //! assertions or per-call response queueing.

    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum ElResponse {
        Valid,
        Syncing,
        Invalid,
    }

    #[derive(Debug, Clone, Copy)]
    struct SyncUpdateCase {
        update: EngineSyncStateUpdate,
        expected_valid_unsafe_number: u64,
        expected_syncing_safe_number_when_allowed: u64,
        expected_syncing_local_safe_number_when_allowed: u64,
        expected_syncing_finalized_number_when_allowed: u64,
    }

    const INITIAL_UNSAFE_NUMBER: u64 = 100;
    const INITIAL_LOCAL_SAFE_NUMBER: u64 = 89;
    const INITIAL_SAFE_NUMBER: u64 = 89;
    const INITIAL_FINALIZED_NUMBER: u64 = 88;

    fn update_case_safe_only() -> SyncUpdateCase {
        SyncUpdateCase {
            update: EngineSyncStateUpdate {
                safe_head: Some(test_block_info(90)),
                ..Default::default()
            },
            expected_valid_unsafe_number: INITIAL_UNSAFE_NUMBER,
            expected_syncing_safe_number_when_allowed: 90,
            expected_syncing_local_safe_number_when_allowed: INITIAL_LOCAL_SAFE_NUMBER,
            expected_syncing_finalized_number_when_allowed: INITIAL_FINALIZED_NUMBER,
        }
    }

    fn update_case_safe_and_local_safe() -> SyncUpdateCase {
        SyncUpdateCase {
            update: EngineSyncStateUpdate {
                local_safe_head: Some(test_block_info(91)),
                safe_head: Some(test_block_info(91)),
                ..Default::default()
            },
            expected_valid_unsafe_number: INITIAL_UNSAFE_NUMBER,
            expected_syncing_safe_number_when_allowed: 91,
            expected_syncing_local_safe_number_when_allowed: 91,
            expected_syncing_finalized_number_when_allowed: INITIAL_FINALIZED_NUMBER,
        }
    }

    fn update_case_safe_local_safe_finalized() -> SyncUpdateCase {
        SyncUpdateCase {
            update: EngineSyncStateUpdate {
                local_safe_head: Some(test_block_info(92)),
                safe_head: Some(test_block_info(92)),
                finalized_head: Some(test_block_info(92)),
                ..Default::default()
            },
            expected_valid_unsafe_number: INITIAL_UNSAFE_NUMBER,
            expected_syncing_safe_number_when_allowed: 92,
            expected_syncing_local_safe_number_when_allowed: 92,
            expected_syncing_finalized_number_when_allowed: 92,
        }
    }

    fn update_case_composite_with_unsafe() -> SyncUpdateCase {
        SyncUpdateCase {
            update: EngineSyncStateUpdate {
                unsafe_head: Some(test_block_info(101)),
                local_safe_head: Some(test_block_info(93)),
                safe_head: Some(test_block_info(93)),
                finalized_head: Some(test_block_info(93)),
            },
            expected_valid_unsafe_number: 101,
            expected_syncing_safe_number_when_allowed: 93,
            expected_syncing_local_safe_number_when_allowed: 93,
            expected_syncing_finalized_number_when_allowed: 93,
        }
    }

    fn fcu_for(response: ElResponse) -> ForkchoiceUpdated {
        match response {
            ElResponse::Valid => valid_fcu(),
            ElResponse::Syncing => syncing_fcu(),
            ElResponse::Invalid => invalid_fcu(),
        }
    }

    #[rstest]
    #[case::safe_only_valid(update_case_safe_only(), ElResponse::Valid, true)]
    #[case::safe_only_syncing_el_synced(update_case_safe_only(), ElResponse::Syncing, true)]
    #[case::safe_only_syncing_el_not_synced(update_case_safe_only(), ElResponse::Syncing, false)]
    #[case::safe_only_invalid(update_case_safe_only(), ElResponse::Invalid, true)]
    #[case::safe_local_valid(update_case_safe_and_local_safe(), ElResponse::Valid, true)]
    #[case::safe_local_syncing_el_synced(
        update_case_safe_and_local_safe(),
        ElResponse::Syncing,
        true
    )]
    #[case::safe_local_syncing_el_not_synced(
        update_case_safe_and_local_safe(),
        ElResponse::Syncing,
        false
    )]
    #[case::safe_local_invalid(update_case_safe_and_local_safe(), ElResponse::Invalid, true)]
    #[case::safe_local_finalized_valid(
        update_case_safe_local_safe_finalized(),
        ElResponse::Valid,
        true
    )]
    #[case::safe_local_finalized_syncing_el_synced(
        update_case_safe_local_safe_finalized(),
        ElResponse::Syncing,
        true
    )]
    #[case::safe_local_finalized_syncing_el_not_synced(
        update_case_safe_local_safe_finalized(),
        ElResponse::Syncing,
        false
    )]
    #[case::safe_local_finalized_invalid(
        update_case_safe_local_safe_finalized(),
        ElResponse::Invalid,
        true
    )]
    #[case::composite_with_unsafe_valid(
        update_case_composite_with_unsafe(),
        ElResponse::Valid,
        true
    )]
    #[case::composite_with_unsafe_syncing_el_synced(
        update_case_composite_with_unsafe(),
        ElResponse::Syncing,
        true
    )]
    #[case::composite_with_unsafe_syncing_el_not_synced(
        update_case_composite_with_unsafe(),
        ElResponse::Syncing,
        false
    )]
    #[case::composite_with_unsafe_invalid(
        update_case_composite_with_unsafe(),
        ElResponse::Invalid,
        true
    )]
    #[tokio::test]
    async fn forkchoice_update_commit_policy_matrix(
        #[case] update_case: SyncUpdateCase,
        #[case] response: ElResponse,
        #[case] el_sync_finished: bool,
    ) {
        let cfg = Arc::new(RollupConfig::default());
        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(fcu_for(response))
                .build(),
        );

        let mut state = TestEngineStateBuilder::new()
            .with_unsafe_head(test_block_info(INITIAL_UNSAFE_NUMBER))
            .with_safe_head(test_block_info(INITIAL_SAFE_NUMBER))
            .with_finalized_head(test_block_info(INITIAL_FINALIZED_NUMBER))
            .with_el_sync_finished(el_sync_finished)
            .build();
        state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
            local_safe_head: Some(test_block_info(INITIAL_LOCAL_SAFE_NUMBER)),
            ..Default::default()
        });
        let pre_state = state;

        let task = SynchronizeTask::new(client, cfg, update_case.update);
        let result = task.execute(&mut state).await;

        match response {
            ElResponse::Valid => {
                assert!(result.is_ok());
                assert_eq!(
                    state.sync_state.unsafe_head().block_info.number,
                    update_case.expected_valid_unsafe_number
                );
                assert_eq!(
                    state.sync_state.safe_head().block_info.number,
                    update_case.expected_syncing_safe_number_when_allowed
                );
                assert_eq!(
                    state.sync_state.local_safe_head().block_info.number,
                    update_case.expected_syncing_local_safe_number_when_allowed
                );
                assert_eq!(
                    state.sync_state.finalized_head().block_info.number,
                    update_case.expected_syncing_finalized_number_when_allowed
                );
            }
            ElResponse::Invalid => {
                assert!(result.is_err());
                assert_eq!(state.sync_state, pre_state.sync_state);
            }
            ElResponse::Syncing => {
                assert!(result.is_ok());
                assert!(
                    state.sync_state.unsafe_head().block_info.number
                        <= pre_state.sync_state.unsafe_head().block_info.number
                );
                if el_sync_finished {
                    assert_eq!(
                        state.sync_state.safe_head().block_info.number,
                        update_case.expected_syncing_safe_number_when_allowed
                    );
                    assert_eq!(
                        state.sync_state.local_safe_head().block_info.number,
                        update_case.expected_syncing_local_safe_number_when_allowed
                    );
                    assert_eq!(
                        state.sync_state.finalized_head().block_info.number,
                        update_case.expected_syncing_finalized_number_when_allowed
                    );
                } else {
                    assert_eq!(state.sync_state.safe_head().block_info.number, INITIAL_SAFE_NUMBER);
                    assert_eq!(
                        state.sync_state.local_safe_head().block_info.number,
                        INITIAL_LOCAL_SAFE_NUMBER
                    );
                    assert_eq!(
                        state.sync_state.finalized_head().block_info.number,
                        INITIAL_FINALIZED_NUMBER
                    );
                }
            }
        }
    }
    }
}
