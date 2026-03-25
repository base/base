use std::sync::Arc;

use alloy_primitives::B256;
use alloy_transport::RpcError;
use base_consensus_rpc::SequencerAdminAPIError;
use base_protocol::{BlockInfo, L2BlockInfo};
use rstest::rstest;
use tokio::sync::oneshot;

use crate::{
    ConductorError, EngineClientError, SequencerAdminQuery,
    actors::{MockConductor, MockSequencerEngineClient, sequencer::tests::test_util::test_actor},
};

#[rstest]
#[tokio::test]
async fn test_is_sequencer_active(
    #[values(true, false)] active: bool,
    #[values(true, false)] via_channel: bool,
) {
    let mut actor = test_actor();
    actor.is_active = active;

    let result = async {
        match via_channel {
            false => actor.is_sequencer_active().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor.handle_admin_query(&mut None, SequencerAdminQuery::SequencerActive(tx)).await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert_eq!(active, result.unwrap());
}

#[rstest]
#[tokio::test]
async fn test_is_conductor_enabled(
    #[values(true, false)] conductor_exists: bool,
    #[values(true, false)] via_channel: bool,
) {
    let mut actor = test_actor();
    if conductor_exists {
        actor.conductor = Some(MockConductor::new())
    };

    let result = async {
        match via_channel {
            false => actor.is_conductor_enabled().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(&mut None, SequencerAdminQuery::ConductorEnabled(tx))
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert_eq!(conductor_exists, result.unwrap());
}

#[rstest]
#[tokio::test]
async fn test_in_recovery_mode(
    #[values(true, false)] recovery_mode: bool,
    #[values(true, false)] via_channel: bool,
) {
    let mut actor = test_actor();
    actor.recovery_mode.set(recovery_mode);

    let result = async {
        match via_channel {
            false => actor.in_recovery_mode().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor.handle_admin_query(&mut None, SequencerAdminQuery::RecoveryMode(tx)).await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert_eq!(recovery_mode, result.unwrap());
}

// --- start_sequencer tests ---

/// No conductor configured: start always succeeds regardless of leadership state.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_no_conductor(
    #[values(true, false)] already_started: bool,
    #[values(true, false)] via_channel: bool,
) {
    let test_hash = B256::from([1u8; 32]);
    let engine_head = L2BlockInfo {
        block_info: BlockInfo { hash: test_hash, ..Default::default() },
        ..Default::default()
    };

    let mut client = MockSequencerEngineClient::new();
    // .returning() (not .return_once()) allows 0 or 1 calls: the already-started case
    // returns early before reaching the engine check.
    client.expect_get_unsafe_head().returning(move || Ok(engine_head));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.is_active = already_started;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(test_hash).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(test_hash, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert!(actor.is_active);
}

/// Conductor confirms leadership: sequencer activates.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_conductor_is_leader(#[values(true, false)] via_channel: bool) {
    let test_hash = B256::from([1u8; 32]);
    let engine_head = L2BlockInfo {
        block_info: BlockInfo { hash: test_hash, ..Default::default() },
        ..Default::default()
    };

    let mut conductor = MockConductor::new();
    conductor.expect_leader().times(1).return_once(|| Ok(true));

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(engine_head));

    let mut actor = test_actor();
    actor.conductor = Some(conductor);
    actor.engine_client = Arc::new(client);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(test_hash).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(test_hash, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert!(actor.is_active);
}

/// Conductor says not leader: sequencer refuses to activate and remains stopped.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_conductor_not_leader(#[values(true, false)] via_channel: bool) {
    let mut conductor = MockConductor::new();
    conductor.expect_leader().times(1).return_once(|| Ok(false));

    let mut actor = test_actor();
    actor.conductor = Some(conductor);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(B256::ZERO).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(B256::ZERO, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(matches!(result.unwrap_err(), SequencerAdminAPIError::NotLeader));
    assert!(!actor.is_active);
}

/// Conductor RPC fails: sequencer refuses to activate and surfaces the error.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_conductor_leader_rpc_error(#[values(true, false)] via_channel: bool) {
    let mut conductor = MockConductor::new();
    conductor
        .expect_leader()
        .times(1)
        .return_once(|| Err(ConductorError::Rpc(RpcError::local_usage_str("rpc error"))));

    let mut actor = test_actor();
    actor.conductor = Some(conductor);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(B256::ZERO).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(B256::ZERO, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(matches!(result.unwrap_err(), SequencerAdminAPIError::RequestError(_)));
    assert!(!actor.is_active);
}

/// Already active: leader check is skipped (idempotent, no RPC call).
#[rstest]
#[tokio::test]
async fn test_start_sequencer_already_active_skips_leader_check(
    #[values(true, false)] via_channel: bool,
) {
    let mut conductor = MockConductor::new();
    // leader() must NOT be called when already active.
    conductor.expect_leader().times(0);

    let mut actor = test_actor();
    actor.conductor = Some(conductor);
    actor.is_active = true;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(B256::ZERO).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(B256::ZERO, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
    assert!(actor.is_active);
}

/// Engine returns a zero hash: sequencer refuses to activate (engine not yet initialized).
#[rstest]
#[tokio::test]
async fn test_start_sequencer_engine_not_initialized(#[values(true, false)] via_channel: bool) {
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(|| Ok(L2BlockInfo::default())); // hash == B256::ZERO

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(B256::from([1u8; 32])).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(B256::from([1u8; 32]), tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(matches!(result.unwrap_err(), SequencerAdminAPIError::RequestError(_)));
    assert!(!actor.is_active);
}

/// Caller's `unsafe_head` does not match the engine's current unsafe head: sequencer refuses.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_unsafe_head_mismatch(#[values(true, false)] via_channel: bool) {
    let requested_hash = B256::from([1u8; 32]);
    let engine_hash = B256::from([2u8; 32]);
    let engine_head = L2BlockInfo {
        block_info: BlockInfo { hash: engine_hash, ..Default::default() },
        ..Default::default()
    };

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(engine_head));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(requested_hash).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(requested_hash, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(matches!(result.unwrap_err(), SequencerAdminAPIError::RequestError(_)));
    assert!(!actor.is_active);
}

/// Engine client returns an error when fetching the unsafe head: sequencer refuses to activate.
#[rstest]
#[tokio::test]
async fn test_start_sequencer_engine_client_error(#[values(true, false)] via_channel: bool) {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(|| Err(EngineClientError::RequestError("rpc failure".to_string())));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.is_active = false;

    let result = async {
        match via_channel {
            false => actor.start_sequencer(B256::from([1u8; 32])).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::StartSequencer(B256::from([1u8; 32]), tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(matches!(result.unwrap_err(), SequencerAdminAPIError::RequestError(_)));
    assert!(!actor.is_active);
}

#[rstest]
#[tokio::test]
async fn test_stop_sequencer_success(
    #[values(true, false)] already_stopped: bool,
    #[values(true, false)] via_channel: bool,
) {
    let unsafe_head = L2BlockInfo {
        block_info: BlockInfo { hash: B256::from([1u8; 32]), ..Default::default() },
        ..Default::default()
    };
    let expected_hash = unsafe_head.hash();

    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).return_once(move || Ok(unsafe_head));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);
    actor.is_active = !already_stopped;

    // verify starting state
    let result = actor.is_sequencer_active().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), !already_stopped);

    // stop the sequencer
    let (tx, rx) = oneshot::channel();
    let mut next_payload = None;
    if via_channel {
        actor.handle_admin_query(&mut next_payload, SequencerAdminQuery::StopSequencer(tx)).await;
    } else {
        actor.stop_sequencer(&mut next_payload, tx).await;
    }
    let result = rx.await.unwrap();
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), expected_hash);

    // verify ending state
    let result = actor.is_sequencer_active().await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[rstest]
#[tokio::test]
async fn test_stop_sequencer_error_fetching_unsafe_head(#[values(true, false)] via_channel: bool) {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_get_unsafe_head()
        .times(1)
        .return_once(|| Err(EngineClientError::RequestError("whoops!".to_string())));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let (tx, rx) = oneshot::channel();
    let mut next_payload = None;
    if via_channel {
        actor.handle_admin_query(&mut next_payload, SequencerAdminQuery::StopSequencer(tx)).await;
    } else {
        actor.stop_sequencer(&mut next_payload, tx).await;
    }
    let result = rx.await.unwrap();
    assert!(result.is_err());

    assert!(matches!(
        result.unwrap_err(),
        SequencerAdminAPIError::ErrorAfterSequencerWasStopped(_)
    ));
    assert!(!actor.is_active);
}

#[rstest]
#[tokio::test]
async fn test_set_recovery_mode(
    #[values(true, false)] starting_mode: bool,
    #[values(true, false)] mode_to_set: bool,
    #[values(true, false)] via_channel: bool,
) {
    let mut actor = test_actor();
    actor.recovery_mode.set(starting_mode);

    // verify starting state
    let result = actor.in_recovery_mode().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), starting_mode);

    // set recovery mode
    let result = async {
        match via_channel {
            false => actor.set_recovery_mode(mode_to_set).await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(
                        &mut None,
                        SequencerAdminQuery::SetRecoveryMode(mode_to_set, tx),
                    )
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;
    assert!(result.is_ok());

    // verify it is set
    let result = actor.in_recovery_mode().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), mode_to_set);
}

#[rstest]
#[tokio::test]
async fn test_override_leader(
    #[values(true, false)] conductor_configured: bool,
    #[values(true, false)] conductor_error: bool,
    #[values(true, false)] via_channel: bool,
) {
    // mock error string returned by conductor, if configured (to differentiate between error
    // returned if not configured)
    let conductor_error_string = "test: error within conductor";

    let mut actor = {
        // wire up conductor absence/presence and response error/success
        if !conductor_configured {
            test_actor()
        } else if conductor_error {
            let mut conductor = MockConductor::new();
            conductor.expect_override_leader().times(1).return_once(move || {
                Err(ConductorError::Rpc(RpcError::local_usage_str(conductor_error_string)))
            });
            let mut actor = test_actor();
            actor.conductor = Some(conductor);
            actor
        } else {
            let mut conductor = MockConductor::new();
            conductor.expect_override_leader().times(1).return_once(|| Ok(()));
            let mut actor = test_actor();
            actor.conductor = Some(conductor);
            actor
        }
    };

    // call to override leader
    let result = async {
        match via_channel {
            false => actor.override_leader().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor.handle_admin_query(&mut None, SequencerAdminQuery::OverrideLeader(tx)).await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    // verify result
    if !conductor_configured || conductor_error {
        assert!(result.is_err());
        assert_eq!(
            conductor_configured,
            result.err().unwrap().to_string().contains(conductor_error_string)
        );
    } else {
        assert!(result.is_ok())
    }
}

#[rstest]
#[tokio::test]
async fn test_reset_derivation_pipeline_success(#[values(true, false)] via_channel: bool) {
    let mut client = MockSequencerEngineClient::new();
    client.expect_reset_engine_forkchoice().times(1).return_once(|| Ok(()));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = async {
        match via_channel {
            false => actor.reset_derivation_pipeline().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(&mut None, SequencerAdminQuery::ResetDerivationPipeline(tx))
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_ok());
}

#[rstest]
#[tokio::test]
async fn test_reset_derivation_pipeline_error(#[values(true, false)] via_channel: bool) {
    let mut client = MockSequencerEngineClient::new();
    client
        .expect_reset_engine_forkchoice()
        .times(1)
        .return_once(|| Err(EngineClientError::RequestError("reset failed".to_string())));

    let mut actor = test_actor();
    actor.engine_client = Arc::new(client);

    let result = async {
        match via_channel {
            false => actor.reset_derivation_pipeline().await,
            true => {
                let (tx, rx) = oneshot::channel();
                actor
                    .handle_admin_query(&mut None, SequencerAdminQuery::ResetDerivationPipeline(tx))
                    .await;
                rx.await.unwrap()
            }
        }
    }
    .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Failed to reset engine"));
}

#[rstest]
#[tokio::test]
async fn test_handle_admin_query_resilient_to_dropped_receiver() {
    let mut conductor = MockConductor::new();
    conductor.expect_override_leader().times(1).returning(|| Ok(()));

    let unsafe_head = L2BlockInfo {
        block_info: BlockInfo { hash: B256::from([1u8; 32]), ..Default::default() },
        ..Default::default()
    };
    let mut client = MockSequencerEngineClient::new();
    client.expect_get_unsafe_head().times(1).returning(move || Ok(unsafe_head));
    client.expect_reset_engine_forkchoice().times(1).returning(|| Ok(()));

    let mut actor = test_actor();
    actor.conductor = Some(conductor);
    actor.engine_client = Arc::new(client);

    let mut queries: Vec<SequencerAdminQuery> = Vec::new();
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::SequencerActive(tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::StartSequencer(B256::ZERO, tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::StopSequencer(tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::ConductorEnabled(tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::RecoveryMode(tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::SetRecoveryMode(true, tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::OverrideLeader(tx));
    }
    {
        // immediately drop receiver
        let (tx, _rx) = oneshot::channel();
        queries.push(SequencerAdminQuery::ResetDerivationPipeline(tx));
    }

    // None of these should fail even if the receiver is dropped
    let mut next_payload = None;
    for query in queries {
        actor.handle_admin_query(&mut next_payload, query).await;
    }
}
