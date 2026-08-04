//! Worker proof submission task metadata and cancellation control.

use std::sync::{Arc, Mutex};

use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::{WorkerSubmitProofRequest, WorkerSubmitProofResponse};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{ClaimedProofJobMetadata, ProofSubmitter, ProofSubmitterError};

/// Claim metadata for a proof submission retained by [`ProofTaskController`].
#[derive(Debug)]
pub struct ProofSubmissionTask {
    /// Claim metadata for the proof job being submitted.
    pub claim: ClaimedProofJobMetadata,
}

impl ProofSubmissionTask {
    /// Creates a submission task handle from claim metadata.
    pub const fn new(claim: ClaimedProofJobMetadata) -> Self {
        Self { claim }
    }
}

/// Cancellation and join control for spawned proof submission tasks.
#[derive(Debug, Clone)]
pub struct ProofTaskController {
    submission_cancel: CancellationToken,
    submissions: Arc<Mutex<JoinSet<Result<WorkerSubmitProofResponse, ProofSubmitterError>>>>,
}

impl ProofTaskController {
    /// Creates a task controller with a fresh submission cancellation token.
    pub fn new() -> Self {
        Self {
            submission_cancel: CancellationToken::new(),
            submissions: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    /// Uses a caller-provided cancellation token for spawned submission tasks.
    #[must_use]
    pub fn with_submission_cancel(mut self, submission_cancel: CancellationToken) -> Self {
        self.submission_cancel = submission_cancel;
        self
    }

    /// Returns the cancellation token used for spawned submission tasks.
    pub const fn submission_cancel(&self) -> &CancellationToken {
        &self.submission_cancel
    }

    /// Returns the number of retained submission tasks that have not been drained.
    pub fn pending_submissions(&self) -> usize {
        self.submissions.lock().expect("submission join set lock should not be poisoned").len()
    }

    /// Cancels spawned submission tasks. Later spawns see an already-cancelled token.
    pub fn cancel_submissions(&self) {
        self.submission_cancel.cancel();
    }

    /// Joins all retained submission tasks.
    pub async fn drain_submissions(&self) {
        let mut submissions = std::mem::take(
            &mut *self.submissions.lock().expect("submission join set lock should not be poisoned"),
        );

        while let Some(result) = submissions.join_next().await {
            Self::log_submission_join_result(result);
        }
    }

    /// Cancels submission tasks and waits for them to finish.
    pub async fn cancel_and_drain_submissions(&self) {
        self.cancel_submissions();
        self.drain_submissions().await;
    }

    /// Spawns proof submission and retains the join handle in this controller.
    pub fn spawn_submission<Client>(
        &self,
        submitter: &ProofSubmitter<Client>,
        request: WorkerSubmitProofRequest,
    ) -> ProofSubmissionTask
    where
        Client: Clone + ProverWorkerProvider + 'static,
    {
        let claim = ClaimedProofJobMetadata {
            session_id: request.session_id.clone(),
            lock_id: request.lock_id.clone(),
            worker_id: request.worker_id.clone(),
        };
        let cancel = self.submission_cancel.clone();
        let submitter = submitter.clone();

        let mut submissions =
            self.submissions.lock().expect("submission join set lock should not be poisoned");
        Self::drain_finished_submissions(&mut submissions);
        submissions.spawn(async move {
            submitter.submit_until_delivered_or_cancelled(request, &cancel).await
        });

        ProofSubmissionTask::new(claim)
    }

    fn drain_finished_submissions(
        submissions: &mut JoinSet<Result<WorkerSubmitProofResponse, ProofSubmitterError>>,
    ) {
        while let Some(result) = submissions.try_join_next() {
            Self::log_submission_join_result(result);
        }
    }

    fn log_submission_join_result(
        result: Result<
            Result<WorkerSubmitProofResponse, ProofSubmitterError>,
            tokio::task::JoinError,
        >,
    ) {
        match result {
            Ok(Ok(_)) | Ok(Err(ProofSubmitterError::Cancelled)) => {}
            Ok(Err(error)) => {
                warn!(error = %error, "proof submission task failed");
            }
            Err(error) => {
                warn!(error = %error, "proof submission task join failed");
            }
        }
    }
}

impl Default for ProofTaskController {
    fn default() -> Self {
        Self::new()
    }
}
