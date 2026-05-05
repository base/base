//! The RPC server for the sequencer actor.
//! Mostly handles queries from the admin rpc.

use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_rpc::{SequencerAdminAPIClient, SequencerAdminAPIError};
use tokio::sync::{mpsc, oneshot};

use super::QueuedRpcClientOptions;
use crate::SequencerAdminQuery;

/// Queued implementation of [`SequencerAdminAPIClient`] that handles requests by sending them to
/// a handler via the contained sender.
#[derive(Debug, Clone)]
pub struct QueuedSequencerAdminAPIClient {
    /// Queue used to relay admin queries
    request_tx: mpsc::Sender<SequencerAdminQuery>,
    /// Runtime options for request/response handling.
    options: QueuedRpcClientOptions,
}

impl QueuedSequencerAdminAPIClient {
    /// Creates a new queued sequencer admin client with historical no-timeout behavior.
    pub fn new(request_tx: mpsc::Sender<SequencerAdminQuery>) -> Self {
        Self::new_with_options(request_tx, QueuedRpcClientOptions::default())
    }

    /// Creates a new queued sequencer admin client with explicit runtime options.
    pub const fn new_with_options(
        request_tx: mpsc::Sender<SequencerAdminQuery>,
        options: QueuedRpcClientOptions,
    ) -> Self {
        Self { request_tx, options }
    }

    /// Awaits a sequencer admin actor response using the configured timeout policy.
    pub async fn receive_admin_response<T>(
        &self,
        response_rx: oneshot::Receiver<Result<T, SequencerAdminAPIError>>,
    ) -> Result<T, SequencerAdminAPIError> {
        let receive = async {
            response_rx.await.map_err(|_| {
                error!(
                    target: "sequencer_admin",
                    "Failed to receive response from sequencer admin rpc"
                );
                SequencerAdminAPIError::ResponseError
            })?
        };

        if let Some(timeout) = self.options.request_timeout {
            tokio::time::timeout(timeout, receive).await.map_err(|_| {
                error!(
                    target: "sequencer_admin",
                    timeout_ms = timeout.as_millis() as u64,
                    "Timed out waiting for sequencer admin rpc response"
                );
                SequencerAdminAPIError::ResponseTimeout
            })?
        } else {
            receive.await
        }
    }
}

#[async_trait]
impl SequencerAdminAPIClient for QueuedSequencerAdminAPIClient {
    async fn is_sequencer_active(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::SequencerActive(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        self.receive_admin_response(rx).await
    }

    async fn is_conductor_enabled(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::ConductorEnabled(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        self.receive_admin_response(rx).await
    }

    async fn is_recovery_mode(&self) -> Result<bool, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::RecoveryMode(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        self.receive_admin_response(rx).await
    }

    async fn start_sequencer(&self, unsafe_head: B256) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::StartSequencer(unsafe_head, tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        self.receive_admin_response(rx).await
    }

    async fn stop_sequencer(&self) -> Result<B256, SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::StopSequencer(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        self.receive_admin_response(rx).await
    }

    async fn set_recovery_mode(&self, mode: bool) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::SetRecoveryMode(mode, tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        self.receive_admin_response(rx).await
    }

    async fn override_leader(&self) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::OverrideLeader(tx)).await.map_err(|_| {
            SequencerAdminAPIError::RequestError("request channel closed".to_string())
        })?;
        self.receive_admin_response(rx).await
    }

    async fn reset_derivation_pipeline(&self) -> Result<(), SequencerAdminAPIError> {
        let (tx, rx) = oneshot::channel();

        self.request_tx.send(SequencerAdminQuery::ResetDerivationPipeline(tx)).await.map_err(
            |_| SequencerAdminAPIError::RequestError("request channel closed".to_string()),
        )?;
        self.receive_admin_response(rx).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn is_sequencer_active_times_out_when_response_is_not_sent() {
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let client = QueuedSequencerAdminAPIClient::new_with_options(
            request_tx,
            QueuedRpcClientOptions { request_timeout: Some(Duration::ZERO) },
        );

        let result = client.is_sequencer_active().await;

        assert!(matches!(result, Err(SequencerAdminAPIError::ResponseTimeout)));
        let request = request_rx.recv().await.expect("request should be queued");
        assert!(
            matches!(request, SequencerAdminQuery::SequencerActive(_)),
            "expected queued sequencer-active query"
        );
    }
}
