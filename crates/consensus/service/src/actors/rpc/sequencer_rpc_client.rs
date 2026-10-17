//! The RPC server for the sequencer actor.
//! Mostly handles queries from the admin rpc.

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_rpc::{SequencerAdminAPIClient, SequencerAdminAPIError};
use derive_more::Constructor;
use tokio::sync::{mpsc, oneshot};

use crate::SequencerAdminQuery;

/// Queued implementation of [`SequencerAdminAPIClient`] that handles requests by sending them to
/// a handler via the contained sender.
#[derive(Debug, Clone, Constructor)]
pub struct QueuedSequencerAdminAPIClient {
    /// Queue used to relay admin queries
    request_tx: mpsc::Sender<SequencerAdminQuery>,
}

impl QueuedSequencerAdminAPIClient {
    async fn query<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, SequencerAdminAPIError>>) -> SequencerAdminQuery,
    ) -> Result<T, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(build(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }
}

#[async_trait]
impl SequencerAdminAPIClient for QueuedSequencerAdminAPIClient {
    async fn is_sequencer_active(&self) -> Result<bool, SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::SequencerActive).await
    }

    async fn is_conductor_enabled(&self) -> Result<bool, SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::ConductorEnabled).await
    }

    async fn is_recovery_mode(&self) -> Result<bool, SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::RecoveryMode).await
    }

    async fn start_sequencer(&self, unsafe_head: B256) -> Result<(), SequencerAdminAPIError> {
        self.query(|tx| SequencerAdminQuery::StartSequencer(unsafe_head, tx)).await
    }

    async fn stop_sequencer(&self) -> Result<B256, SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::StopSequencer).await
    }

    async fn set_recovery_mode(&self, mode: bool) -> Result<(), SequencerAdminAPIError> {
        self.query(|tx| SequencerAdminQuery::SetRecoveryMode(mode, tx)).await
    }

    async fn override_leader(&self) -> Result<(), SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::OverrideLeader).await
    }

    async fn reset_derivation_pipeline(&self) -> Result<(), SequencerAdminAPIError> {
        self.query(SequencerAdminQuery::ResetDerivationPipeline).await
    }
}
