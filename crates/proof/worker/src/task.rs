//! Worker proof submission task metadata and cancellation control.

use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::{WorkerSubmitProofRequest, WorkerSubmitProofResponse};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{ClaimedProofJobMetadata, ProofSubmitter, ProofSubmitterError};

/// Handle for a proof submission task spawned after successful proof generation.
#[derive(Debug)]
pub struct ProofSubmissionTask {
    /// Claim metadata for the proof job being submitted.
    pub claim: ClaimedProofJobMetadata,
    /// Spawned proof submission task.
    pub submit_handle: JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>>,
}

impl ProofSubmissionTask {
    /// Creates a submission task handle from claim metadata and a spawned task.
    pub const fn new(
        claim: ClaimedProofJobMetadata,
        submit_handle: JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>>,
    ) -> Self {
        Self { claim, submit_handle }
    }
}

/// Cancellation control for proof generation and submission handoff.
#[derive(Debug, Clone)]
pub struct ProofTaskController {
    submission_cancel: CancellationToken,
}

impl ProofTaskController {
    /// Creates a task controller with a fresh submission cancellation token.
    pub fn new() -> Self {
        Self { submission_cancel: CancellationToken::new() }
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

    /// Signals spawned submission tasks to stop during worker shutdown.
    ///
    /// This permanently cancels the controller's submission token. Treat the
    /// controller as shutdown-only after calling this method; later calls to
    /// [`Self::spawn_submission`] will create immediately-cancelled tasks.
    pub fn cancel_submissions(&self) {
        self.submission_cancel.cancel();
    }

    /// Spawns proof submission using the controller's cancellation token.
    ///
    /// Do not call this after [`Self::cancel_submissions`], because the shared
    /// cancellation token is permanently cancelled during shutdown.
    #[must_use = "dropping the JoinHandle detaches the submission task"]
    pub fn spawn_submission<Client>(
        &self,
        submitter: &ProofSubmitter<Client>,
        request: WorkerSubmitProofRequest,
    ) -> JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>>
    where
        Client: Clone + ProverWorkerProvider + 'static,
    {
        submitter.spawn_until_delivered(request, self.submission_cancel.clone())
    }
}

impl Default for ProofTaskController {
    fn default() -> Self {
        Self::new()
    }
}
