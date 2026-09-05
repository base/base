//! Worker proof submission task cancellation control.

use std::sync::Arc;

use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::{WorkerSubmitProofRequest, WorkerSubmitProofResponse};
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, warn};

use crate::{DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS, ProofSubmitter, ProofSubmitterError};

/// Default maximum number of in-flight proof submission tasks.
pub const DEFAULT_MAX_PENDING_SUBMISSIONS: usize = DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS;

/// Default grace period to wait for cancelled submissions before aborting them.
pub const DEFAULT_SUBMISSION_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Cancellation and join control for spawned proof submission tasks.
#[derive(Debug, Clone)]
pub struct ProofTaskController {
    submission_cancel: CancellationToken,
    submissions: Arc<Mutex<JoinSet<Result<WorkerSubmitProofResponse, ProofSubmitterError>>>>,
    submission_permits: Arc<Semaphore>,
    shutdown_grace: Duration,
}

impl ProofTaskController {
    /// Creates a task controller with a fresh submission cancellation token.
    pub fn new() -> Self {
        Self {
            submission_cancel: CancellationToken::new(),
            submissions: Arc::new(Mutex::new(JoinSet::new())),
            submission_permits: Arc::new(Semaphore::new(DEFAULT_MAX_PENDING_SUBMISSIONS)),
            shutdown_grace: DEFAULT_SUBMISSION_SHUTDOWN_GRACE,
        }
    }

    /// Limits how many submission tasks may run at once.
    #[must_use]
    pub fn with_max_pending_submissions(mut self, max_pending: usize) -> Self {
        let max_pending = max_pending.max(1);
        self.submission_permits = Arc::new(Semaphore::new(max_pending));
        self
    }

    /// Returns the number of retained submission tasks that have not been drained.
    pub fn pending_submissions(&self) -> usize {
        self.submissions.try_lock().map(|submissions| submissions.len()).unwrap_or(0)
    }

    /// Cancels spawned submission tasks. Later spawns see an already-cancelled token.
    pub fn cancel_submissions(&self) {
        self.submission_cancel.cancel();
    }

    /// Joins retained submission tasks, aborting any still running after the grace period.
    ///
    /// Concurrent callers serialize on the submission lock: the first drain joins (or aborts)
    /// current tasks, and later callers wait then observe an empty set.
    pub async fn drain_submissions(&self) {
        let mut submissions = self.submissions.lock().await;

        if timeout(self.shutdown_grace, async {
            while let Some(result) = submissions.join_next().await {
                Self::log_submission_join_result(result);
            }
        })
        .await
        .is_err()
        {
            warn!(
                grace_ms = self.shutdown_grace.as_millis(),
                pending = submissions.len(),
                "submission drain exceeded grace period; aborting remaining tasks"
            );
            submissions.abort_all();
            while let Some(result) = submissions.join_next().await {
                Self::log_submission_join_result(result);
            }
        }
    }

    /// Cancels submission tasks and waits for them to finish (or abort after grace).
    pub async fn cancel_and_drain_submissions(&self) {
        self.cancel_submissions();
        self.drain_submissions().await;
    }

    /// Acquires a pending-submission permit.
    pub async fn acquire_submission_permit(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.submission_permits)
            .acquire_owned()
            .await
            .expect("submission permit semaphore should not be closed")
    }

    /// Spawns proof submission after acquiring a pending-submission permit.
    pub async fn spawn_submission<Client>(
        &self,
        submitter: &ProofSubmitter<Client>,
        request: WorkerSubmitProofRequest,
    ) where
        Client: Clone + ProverWorkerProvider + 'static,
    {
        let permit = self.acquire_submission_permit().await;
        self.spawn_submission_with_permit(submitter, request, permit).await;
    }

    /// Spawns proof submission with an already-acquired permit.
    pub async fn spawn_submission_with_permit<Client>(
        &self,
        submitter: &ProofSubmitter<Client>,
        request: WorkerSubmitProofRequest,
        permit: OwnedSemaphorePermit,
    ) where
        Client: Clone + ProverWorkerProvider + 'static,
    {
        let cancel = self.submission_cancel.clone();
        let submitter = submitter.clone();
        let span = tracing::Span::current();

        let mut submissions = self.submissions.lock().await;
        Self::drain_finished_submissions(&mut submissions);
        submissions.spawn(async move {
            let _permit = permit;
            submitter.submit_until_delivered_or_cancelled(request, &cancel).instrument(span).await
        });
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
            Err(error) if error.is_cancelled() => {}
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
