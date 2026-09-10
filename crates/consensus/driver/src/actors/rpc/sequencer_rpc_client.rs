//! The RPC server for the sequencer actor.
//! Mostly handles queries from the admin rpc.

use alloy_primitives::B256;
use derive_more::Constructor;
use tokio::sync::{mpsc, oneshot};

use crate::{SequencerAdminAPIError, SequencerAdminQuery};

/// Sends sequencer administration requests to the driver.
#[derive(Debug, Clone, Constructor)]
pub struct SequencerAdminClient {
    /// Queue used to relay admin queries
    request_tx: mpsc::Sender<SequencerAdminQuery>,
}

impl SequencerAdminClient {
    /// Check if the sequencer is active.
    pub async fn is_sequencer_active(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::SequencerActive(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Check if the conductor is enabled.
    pub async fn is_conductor_enabled(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::ConductorEnabled(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Check if in recovery mode.
    pub async fn is_recovery_mode(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::RecoveryMode(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Start the sequencer.
    pub async fn start_sequencer(&self, unsafe_head: B256) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::StartSequencer(unsafe_head, tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Stop the sequencer.
    pub async fn stop_sequencer(&self) -> Result<B256, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::StopSequencer(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Set recovery mode.
    pub async fn set_recovery_mode(&self, mode: bool) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::SetRecoveryMode(mode, tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Override the leader.
    pub async fn override_leader(&self) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::OverrideLeader(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }

    /// Reset the derivation pipeline.
    pub async fn reset_derivation_pipeline(&self) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::ResetDerivationPipeline(tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        rx.await.map_err(|_| SequencerAdminAPIError::ResponseError)?
    }
}
