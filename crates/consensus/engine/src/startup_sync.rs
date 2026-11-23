//! Target-specific execution readiness for inactive sequencer startup.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use alloy_transport::TransportError;
use base_protocol::L2BlockInfo;
use thiserror::Error;

use crate::EngineClient;

/// Execution readiness of one selected startup target, not permission to produce blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupSyncStatus {
    /// The target has not received a successful forkchoice confirmation.
    Pending,
    /// The latest probe returned VALID for the exact selected target.
    ExecutionReady,
}

/// Failure of one startup probe. The caller owns retry classification and its overall deadline.
#[derive(Debug, Error)]
pub enum StartupSyncError {
    /// The execution request exceeded its remaining caller budget.
    #[error("startup forkchoice confirmation timed out")]
    Timeout,
    /// The execution client failed. Preserve the transport or RPC error for classification.
    #[error("startup forkchoice request failed: {0}")]
    Request(#[source] TransportError),
    /// FCU returned a status other than VALID or SYNCING.
    #[error("unexpected startup forkchoice status: {0}")]
    UnexpectedStatus(PayloadStatusEnum),
}

/// Target-specific EL confirmation for an inactive sequencer.
///
/// This is a startup protocol building block, not a production activation check. The caller must
/// authenticate targets, serialize engine access, keep production stopped, and initialize the
/// derivation anchor before activation. It neither updates consensus-side engine labels nor reuses
/// the engine's
/// process-wide EL completion latch. Do not call concurrently with normal forkchoice processing.
#[derive(Debug)]
pub struct SequencerStartupSync {
    target: L2BlockInfo,
    status: StartupSyncStatus,
}

impl SequencerStartupSync {
    /// Starts unconfirmed at a caller-authenticated target.
    pub const fn new(target: L2BlockInfo) -> Self {
        Self { target, status: StartupSyncStatus::Pending }
    }

    /// Returns the caller-authenticated target whose execution is being confirmed.
    pub const fn target(&self) -> L2BlockInfo {
        self.target
    }

    /// Returns execution readiness only, without authorizing leadership or block production.
    pub const fn status(&self) -> StartupSyncStatus {
        self.status
    }

    /// Replaces the caller-authenticated target and invalidates any previous confirmation.
    pub const fn set_target(&mut self, target: L2BlockInfo) {
        self.target = target;
        self.status = StartupSyncStatus::Pending;
    }

    /// Confirms the selected target with one attribute-free FCU within the remaining caller budget.
    ///
    /// Every call reaches the EL, including repeated targets. Cancellation, timeout, RPC failures
    /// and unexpected statuses leave readiness pending. A timeout may follow an EL side effect;
    /// callers must reconfirm state rather than assume the request did not execute. No automatic
    /// retries or engine resets occur here.
    pub async fn probe<C: EngineClient>(
        &mut self,
        client: &C,
        remaining: Duration,
    ) -> Result<StartupSyncStatus, StartupSyncError> {
        self.status = StartupSyncStatus::Pending;
        if remaining.is_zero() {
            return Err(StartupSyncError::Timeout);
        }
        let forkchoice = ForkchoiceState {
            head_block_hash: self.target.block_info.hash,
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };
        let response = tokio::time::timeout(
            remaining, client.fork_choice_updated_v3(forkchoice, None),
        ).await.map_err(|_| StartupSyncError::Timeout)?
            .map_err(StartupSyncError::Request)?;
        match response.payload_status.status {
            PayloadStatusEnum::Valid => self.status = StartupSyncStatus::ExecutionReady,
            PayloadStatusEnum::Syncing => {}
            status => return Err(StartupSyncError::UnexpectedStatus(status)),
        }
        Ok(self.status)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};

    use super::{SequencerStartupSync, StartupSyncStatus};
    use crate::test_utils::{test_block_info, test_engine_client_builder};

    /// Builds an explicit Engine API status response for the scripted client.
    fn response(status: PayloadStatusEnum) -> ForkchoiceUpdated {
        ForkchoiceUpdated { payload_status: PayloadStatus { status, latest_valid_hash: None }, payload_id: None }
    }

    /// A pending or failed request must not retain readiness from an earlier confirmation.
    #[tokio::test]
    async fn startup_sync_timeout_and_rpc_failure_revoke_readiness() {
        let client = test_engine_client_builder()
            .with_fork_choice_updated_v3_response(response(PayloadStatusEnum::Valid)).build();
        let mut startup = SequencerStartupSync::new(test_block_info(100));
        startup.probe(&client, Duration::from_secs(1)).await.unwrap();
        let storage = client.storage();
        let mut guard = storage.write().await;
        assert!(matches!(startup.probe(&client, Duration::from_millis(1)).await, Err(super::StartupSyncError::Timeout)), "a blocked request must time out");
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "a timeout must revoke an earlier VALID");
        guard.fork_choice_updated_v3_response = None;
        drop(guard);
        assert!(matches!(startup.probe(&client, Duration::from_secs(1)).await, Err(super::StartupSyncError::Request(_))), "the original RPC failure must remain distinguishable from a timeout or status failure");
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "an RPC error must not restore readiness");
    }

    /// An exhausted budget must not send even an immediately completing FCU.
    #[tokio::test]
    async fn expired_startup_budget_does_not_send_fcu() {
        let client = test_engine_client_builder()
            .with_fork_choice_updated_v3_response(response(PayloadStatusEnum::Valid)).build();
        let mut startup = SequencerStartupSync::new(test_block_info(100));
        assert!(matches!(startup.probe(&client, Duration::ZERO).await, Err(super::StartupSyncError::Timeout)), "an exhausted caller budget must fail before sending");
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "an expired budget cannot confer readiness");
        assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), 0, "no request may be sent without a caller budget");
    }

    /// The exact same target must be probed again after SYNCING, without relying on a latch.
    #[tokio::test]
    async fn startup_sync_reprobes_and_invalidates_readiness() {
        let target = test_block_info(100);
        let client = test_engine_client_builder().with_fork_choice_updated_v3_response(
            response(PayloadStatusEnum::Syncing)
        ).build();
        let mut startup = SequencerStartupSync::new(target);
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "new target is not ready");
        assert_eq!(startup.probe(&client, Duration::from_secs(1)).await.unwrap(), StartupSyncStatus::Pending, "SYNCING cannot authorize a handoff");
        client.set_fork_choice_updated_v3_response(response(PayloadStatusEnum::Valid)).await;
        assert_eq!(startup.probe(&client, Duration::from_secs(1)).await.unwrap(), StartupSyncStatus::ExecutionReady, "VALID confirms this target only");
        let requests = client.storage().read().await.fork_choice_updated_v3_requests.clone();
        assert_eq!(requests.len(), 2, "the unchanged target must receive a second FCU");
        for (forkchoice, has_attributes) in requests {
            assert_eq!(forkchoice.head_block_hash, target.block_info.hash, "FCU must name the selected target");
            assert_eq!(forkchoice.safe_block_hash, alloy_primitives::B256::ZERO, "startup must not invent a safe label");
            assert_eq!(forkchoice.finalized_block_hash, alloy_primitives::B256::ZERO, "startup must not invent finality");
            assert!(!has_attributes, "catch-up must not initiate a payload build");
        }
        startup.set_target(test_block_info(200));
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "a second target must invalidate the first confirmation");
        client.set_fork_choice_updated_v3_response(response(PayloadStatusEnum::Accepted)).await;
        assert!(startup.probe(&client, Duration::from_secs(1)).await.is_err(), "ACCEPTED is not a valid FCU readiness response");
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "an unexpected response must fail closed");
    }

    /// An unsuccessful confirmation attempt revokes readiness even for an unchanged target.
    #[tokio::test]
    async fn startup_sync_never_reuses_a_previous_valid_response() {
        let client = test_engine_client_builder().with_fork_choice_updated_v3_response(
            response(PayloadStatusEnum::Valid)
        ).build();
        let mut startup = SequencerStartupSync::new(test_block_info(100));
        startup.probe(&client, Duration::from_secs(1)).await.unwrap();
        client.set_fork_choice_updated_v3_response(response(PayloadStatusEnum::Invalid { validation_error: "invalid target".into() })).await;
        assert!(startup.probe(&client, Duration::from_secs(1)).await.is_err(), "INVALID must fail the confirmation");
        assert_eq!(startup.status(), StartupSyncStatus::Pending, "previous VALID is not a permanent readiness latch");
    }
}
