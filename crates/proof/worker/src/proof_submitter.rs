//! Proof submission types for prover-service worker delivery.
//!
//! [`ProofSubmitter`] is backend-neutral: hosts build a
//! `WorkerSubmitProofRequest` from their own proof result type, then hand the
//! request to this shared worker component for delivery.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use backon::Retryable;
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    HeartbeatRequest, HeartbeatResponse, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use base_retry::{DEFAULT_UNBOUNDED_INITIAL_DELAY, DEFAULT_UNBOUNDED_MAX_DELAY, RetryConfig};
use thiserror::Error;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Errors raised while preparing or submitting a generated proof.
#[derive(Debug, Error)]
pub enum ProofSubmitterError {
    /// The generated proof result is not one this worker can submit.
    #[error("proof submitter received an unsupported proof result")]
    UnsupportedProofResult,
    /// Proof submission was cancelled before delivery.
    #[error("proof submission cancelled before delivery")]
    Cancelled,
    /// Prover service worker API submission failed.
    #[error(transparent)]
    Submit(#[from] ProverServiceClientError),
}

impl ProofSubmitterError {
    /// Returns `true` when retrying the submission may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::UnsupportedProofResult | Self::Cancelled => false,
            Self::Submit(error) => error.is_retryable(),
        }
    }
}

/// Submitter for delivering generated proofs to the prover-service worker API.
#[derive(Clone, Debug)]
pub struct ProofSubmitter<Client> {
    client: Client,
    backoff: RetryConfig,
}

impl<Client> ProofSubmitter<Client> {
    /// Creates a proof submitter using the default backoff config.
    pub const fn new(client: Client) -> Self {
        Self {
            client,
            backoff: RetryConfig::unbounded(
                DEFAULT_UNBOUNDED_INITIAL_DELAY,
                DEFAULT_UNBOUNDED_MAX_DELAY,
            ),
        }
    }

    /// Sets the retry backoff config.
    pub const fn with_backoff_config(mut self, backoff: RetryConfig) -> Self {
        self.backoff = backoff;
        self
    }

    /// Returns the configured retry backoff.
    pub const fn backoff_config(&self) -> RetryConfig {
        self.backoff
    }

    /// Returns the underlying worker client.
    pub const fn client(&self) -> &Client {
        &self.client
    }
}

impl<Client> ProofSubmitter<Client>
where
    Client: ProverWorkerProvider,
{
    /// Extend a claimed proof job lock through the worker API.
    ///
    /// Returns the raw client error so heartbeat policy can distinguish
    /// retryable and permanent worker API failures without wrapping.
    pub async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError> {
        let session_id = request.session_id.clone();
        let lock_id = request.lock_id.clone();
        let worker_id = request.worker_id.clone();
        let lock_duration_seconds = request.lock_duration_seconds;

        match self.client.heartbeat(request).await {
            Ok(response) => {
                debug!(
                    session_id = %session_id,
                    lock_id = %lock_id,
                    worker_id = %worker_id,
                    lock_duration_seconds = lock_duration_seconds,
                    status = ?response.job.status,
                    lock_expires_at = ?response.job.lock_expires_at,
                    "proof job heartbeat delivered"
                );
                Ok(response)
            }
            Err(error) => {
                warn!(
                    session_id = %session_id,
                    lock_id = %lock_id,
                    worker_id = %worker_id,
                    lock_duration_seconds = lock_duration_seconds,
                    error = %error,
                    "proof job heartbeat failed"
                );
                Err(error)
            }
        }
    }

    /// Submits a generated proof through the worker API once.
    pub async fn submit_once(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProofSubmitterError> {
        let session_id = request.session_id.clone();
        let lock_id = request.lock_id.clone();
        let worker_id = request.worker_id.clone();

        match self.client.submit_proof(request).await {
            Ok(response) => {
                info!(
                    session_id = %session_id,
                    lock_id = %lock_id,
                    worker_id = %worker_id,
                    "proof submission delivered"
                );
                Ok(response)
            }
            Err(error) => {
                warn!(
                    session_id = %session_id,
                    lock_id = %lock_id,
                    worker_id = %worker_id,
                    error = %error,
                    "proof submission failed"
                );
                Err(ProofSubmitterError::Submit(error))
            }
        }
    }

    /// Submits a generated proof through the worker API until delivered or permanently rejected.
    ///
    /// This adds a long-lived delivery loop around the worker client's per-call retry budget. Each
    /// delivery attempt delegates to the client once; concrete clients may perform bounded RPC
    /// retries internally before returning a retryable error to this loop.
    pub async fn submit_until_delivered(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProofSubmitterError> {
        let cancel = CancellationToken::new();
        self.submit_until_delivered_or_cancelled(request, &cancel).await
    }

    /// Submits a generated proof until success or cooperative cancellation.
    ///
    /// Cancellation is checked between submission attempts so an in-flight RPC can complete.
    pub async fn submit_until_delivered_or_cancelled(
        &self,
        request: WorkerSubmitProofRequest,
        cancel: &CancellationToken,
    ) -> Result<WorkerSubmitProofResponse, ProofSubmitterError> {
        let delivery_attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_submit = Arc::clone(&delivery_attempts);
        let cancel = cancel.clone();

        let response = (|| {
            let request = request.clone();
            let attempts = Arc::clone(&attempts_for_submit);
            let cancel = cancel.clone();

            async move {
                if cancel.is_cancelled() {
                    return Err(ProofSubmitterError::Cancelled);
                }

                attempts.fetch_add(1, Ordering::Relaxed);
                match self.client.submit_proof(request).await {
                    Ok(response) => Ok(response),
                    Err(error) if cancel.is_cancelled() && error.is_retryable() => {
                        Err(ProofSubmitterError::Cancelled)
                    }
                    Err(error) => Err(ProofSubmitterError::Submit(error)),
                }
            }
        })
        .retry(self.backoff.to_backoff_builder())
        .sleep({
            let cancel = cancel.clone();
            move |delay| {
                let cancel = cancel.clone();

                async move {
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = sleep(delay) => {}
                    }
                }
            }
        })
        .when({
            let cancel = cancel.clone();
            move |error: &ProofSubmitterError| !cancel.is_cancelled() && error.is_retryable()
        })
        .notify(|error, delay| {
            if let ProofSubmitterError::Submit(error) = error {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    backoff_ms = delay.as_millis(),
                    error = %error,
                    "proof submission retry window exhausted; retrying"
                );
            }
        })
        .await;

        match response {
            Ok(response) => {
                info!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    "proof submission delivered"
                );
                Ok(response)
            }
            Err(ProofSubmitterError::Cancelled) => {
                info!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    "proof submission cancelled"
                );
                Err(ProofSubmitterError::Cancelled)
            }
            Err(ProofSubmitterError::Submit(error))
                if cancel.is_cancelled() && error.is_retryable() =>
            {
                info!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    "proof submission cancelled"
                );
                Err(ProofSubmitterError::Cancelled)
            }
            Err(ProofSubmitterError::Submit(error)) => {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    error = %error,
                    "proof submission failed permanently"
                );
                Err(ProofSubmitterError::Submit(error))
            }
            Err(ProofSubmitterError::UnsupportedProofResult) => {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    delivery_attempts = delivery_attempts.load(Ordering::Relaxed),
                    "proof submission failed: unsupported proof result"
                );
                Err(ProofSubmitterError::UnsupportedProofResult)
            }
        }
    }
}

impl<Client> ProofSubmitter<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Spawns proof submission as an async Tokio task.
    #[must_use = "dropping the JoinHandle detaches the submission task"]
    pub fn spawn_until_delivered(
        &self,
        request: WorkerSubmitProofRequest,
        cancel: CancellationToken,
    ) -> JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>> {
        let submitter = self.clone();
        tokio::spawn(async move {
            submitter.submit_until_delivered_or_cancelled(request, &cancel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
        ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult,
        RecordProofSessionRequest, RecordProofSessionResponse, ZkProofRequest, ZkProofResult, ZkVm,
    };
    use chrono::Utc;
    use tokio::{sync::Notify, time::timeout};

    use super::*;
    use crate::ProofTaskController;

    #[derive(Clone, Debug)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
        submission_notify: Arc<Notify>,
    }

    #[derive(Debug)]
    struct MockWorkerState {
        failures: Vec<ProverServiceClientError>,
        submissions: Vec<WorkerSubmitProofRequest>,
    }

    impl MockWorkerClient {
        fn new(failures: Vec<ProverServiceClientError>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState { failures, submissions: Vec::new() })),
                submission_notify: Arc::new(Notify::new()),
            }
        }

        fn submission_count(&self) -> usize {
            self.state.lock().expect("mock state poisoned").submissions.len()
        }

        async fn wait_for_submission_count(&self, count: usize) {
            loop {
                let notified = self.submission_notify.notified();
                if self.submission_count() >= count {
                    return;
                }
                notified.await;
            }
        }
    }

    #[async_trait]
    impl ProverWorkerProvider for MockWorkerClient {
        async fn get_next_proof(
            &self,
            _request: GetNextProofRequest,
        ) -> Result<GetNextProofResponse, ProverServiceClientError> {
            panic!("get_next_proof is not used by proof submitter tests")
        }

        async fn heartbeat(
            &self,
            _request: HeartbeatRequest,
        ) -> Result<HeartbeatResponse, ProverServiceClientError> {
            panic!("heartbeat is not used by proof submitter tests")
        }

        async fn submit_proof(
            &self,
            request: WorkerSubmitProofRequest,
        ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
            let mut state = self.state.lock().expect("mock state poisoned");
            state.submissions.push(request.clone());
            self.submission_notify.notify_waiters();
            if !state.failures.is_empty() {
                return Err(state.failures.remove(0));
            }

            Ok(WorkerSubmitProofResponse { job: proof_job_for_submission(&request) })
        }

        async fn get_proof_session(
            &self,
            _request: GetProofSessionRequest,
        ) -> Result<GetProofSessionResponse, ProverServiceClientError> {
            panic!("get_proof_session is not used by proof submitter tests")
        }

        async fn record_proof_session(
            &self,
            _request: RecordProofSessionRequest,
        ) -> Result<RecordProofSessionResponse, ProverServiceClientError> {
            panic!("record_proof_session is not used by proof submitter tests")
        }
    }

    fn retryable_error() -> ProverServiceClientError {
        ProverServiceClientError::Timeout("service unavailable".to_owned())
    }

    fn non_retryable_error() -> ProverServiceClientError {
        ProverServiceClientError::WorkerLeaseRejected {
            message: "proof job lock is not owned by worker".to_owned(),
        }
    }

    fn submit_request() -> WorkerSubmitProofRequest {
        WorkerSubmitProofRequest {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
            result: ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![1, 2, 3].into(),
            }),
        }
    }

    fn proof_job_for_submission(request: &WorkerSubmitProofRequest) -> ProofJob {
        let now = Utc::now();
        ProofJob {
            session_id: request.session_id.clone(),
            status: ProofJobStatus::Succeeded,
            request: ProofRequest {
                session_id: request.session_id.clone(),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: 1,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    zk_vm: ZkVm::Sp1,
                }),
            },
            attempt: 1,
            lock_id: Some(request.lock_id.clone()),
            worker_id: Some(request.worker_id.clone()),
            lock_expires_at: None,
            created_at: now,
            updated_at: now,
            completed_at: Some(now),
            error_message: None,
        }
    }

    #[tokio::test]
    async fn submitter_delivers_successfully() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());

        let response =
            submitter.submit_once(submit_request()).await.expect("submission should succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 1);
    }

    fn fast_backoff() -> RetryConfig {
        RetryConfig::unbounded(Duration::from_millis(1), Duration::from_millis(1))
    }

    #[tokio::test]
    async fn submitter_retries_retryable_failures_until_delivered() {
        let client = MockWorkerClient::new(vec![retryable_error(), retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(fast_backoff());

        let response = submitter
            .submit_until_delivered(submit_request())
            .await
            .expect("retryable failures should eventually deliver");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 3);
    }

    #[tokio::test]
    async fn submitter_stops_on_non_retryable_error() {
        let client = MockWorkerClient::new(vec![non_retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(fast_backoff());

        let result = submitter.submit_until_delivered(submit_request()).await;

        assert!(matches!(result, Err(ProofSubmitterError::Submit(_))));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn submitter_stops_when_cancelled_before_submission() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = submitter.submit_until_delivered_or_cancelled(submit_request(), &cancel).await;

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 0);
    }

    #[tokio::test]
    async fn submitter_stops_when_cancelled_during_retry_backoff() {
        let client = MockWorkerClient::new(vec![retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            RetryConfig::unbounded(Duration::from_secs(60), Duration::from_secs(60)),
        );
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let submitter = submitter.clone();
            let cancel = cancel.clone();
            async move { submitter.submit_until_delivered_or_cancelled(submit_request(), &cancel).await }
        });

        timeout(Duration::from_secs(1), client.wait_for_submission_count(1))
            .await
            .expect("first submission attempt should happen");
        cancel.cancel();

        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("submission task should finish after cancellation")
            .expect("submission task should not panic");

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn spawned_submitter_delivers_successfully() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());

        let handle = submitter.spawn_until_delivered(submit_request(), CancellationToken::new());
        let response = handle
            .await
            .expect("submission task should not panic")
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn spawned_submitter_stops_when_cancelled_before_submission() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let handle = submitter.spawn_until_delivered(submit_request(), cancel);
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancelled submission task should finish")
            .expect("submission task should not panic");

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 0);
    }

    #[tokio::test]
    async fn task_controller_cancels_spawned_submission() {
        let client = MockWorkerClient::new(vec![retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            RetryConfig::unbounded(Duration::from_secs(60), Duration::from_secs(60)),
        );
        let controller = ProofTaskController::new();

        let handle = controller.spawn_submission(&submitter, submit_request());
        timeout(Duration::from_secs(1), client.wait_for_submission_count(1))
            .await
            .expect("first submission attempt should happen");
        controller.cancel_submissions();

        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancelled submission task should finish")
            .expect("submission task should not panic");

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 1);
    }
}
