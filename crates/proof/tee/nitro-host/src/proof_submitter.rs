//! Async proof submission task for prover-service worker delivery.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use backon::{ExponentialBuilder, Retryable};
use base_proof_primitives::ProofResult as NitroProofResult;
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    HeartbeatRequest, HeartbeatResponse, ProofResult as ServiceProofResult, TeeKind,
    TeeProofResult, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use thiserror::Error;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Minimum delay used to avoid tight retry loops.
pub const MIN_PROOF_SUBMITTER_BACKOFF: Duration = Duration::from_millis(1);

/// Default initial retry delay for proof submission.
pub const DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Default maximum retry delay for proof submission.
pub const DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Exponential backoff configuration for proof submission retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofSubmitterBackoffConfig {
    /// First delay after a retryable submission failure.
    pub initial_delay: Duration,
    /// Maximum delay between retry attempts.
    pub max_delay: Duration,
}

impl ProofSubmitterBackoffConfig {
    /// Creates a proof submission backoff config.
    pub const fn new(initial_delay: Duration, max_delay: Duration) -> Self {
        Self { initial_delay, max_delay }
    }

    /// Returns the configured max delay, clamped to the minimum allowed delay.
    pub fn normalized_max_delay(&self) -> Duration {
        self.max_delay.max(MIN_PROOF_SUBMITTER_BACKOFF)
    }

    /// Returns the configured initial delay, clamped to the configured max delay.
    pub fn normalized_initial_delay(&self) -> Duration {
        self.initial_delay.max(MIN_PROOF_SUBMITTER_BACKOFF).min(self.normalized_max_delay())
    }

    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.normalized_initial_delay())
            .with_max_delay(self.normalized_max_delay())
            .without_max_times()
            .with_jitter()
    }
}

impl Default for ProofSubmitterBackoffConfig {
    fn default() -> Self {
        Self::new(DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF, DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF)
    }
}

/// Errors raised while preparing or submitting a generated proof.
#[derive(Debug, Error)]
pub enum ProofSubmitterError {
    /// Nitro proof submitter only submits TEE proof results.
    #[error("nitro proof submitter only accepts TEE proof results")]
    UnsupportedProofResult,
    /// Proof submission was cancelled before delivery.
    #[error("proof submission cancelled before delivery")]
    Cancelled,
    /// Prover service worker API submission failed.
    #[error(transparent)]
    Submit(#[from] ProverServiceClientError),
}

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct ProofSubmitterRequest;

impl ProofSubmitterRequest {
    /// Builds a worker proof submission request from a generated Nitro TEE proof.
    pub fn from_tee_proof(
        session_id: String,
        lock_id: String,
        worker_id: String,
        proof: NitroProofResult,
    ) -> Result<WorkerSubmitProofRequest, ProofSubmitterError> {
        let NitroProofResult::Tee { aggregate_proposal, proposals } = proof else {
            return Err(ProofSubmitterError::UnsupportedProofResult);
        };

        Ok(WorkerSubmitProofRequest {
            session_id,
            lock_id,
            worker_id,
            result: ServiceProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::AwsNitro,
            }),
        })
    }
}

/// Submitter for delivering generated proofs to the prover-service worker API.
#[derive(Clone, Debug)]
pub struct ProofSubmitter<Client> {
    client: Client,
    backoff: ProofSubmitterBackoffConfig,
}

impl<Client> ProofSubmitter<Client> {
    /// Creates a proof submitter using the default backoff config.
    pub const fn new(client: Client) -> Self {
        Self {
            client,
            backoff: ProofSubmitterBackoffConfig::new(
                DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF,
                DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF,
            ),
        }
    }

    /// Sets the retry backoff config.
    pub const fn with_backoff_config(mut self, backoff: ProofSubmitterBackoffConfig) -> Self {
        self.backoff = backoff;
        self
    }

    /// Returns the configured retry backoff.
    pub const fn backoff_config(&self) -> ProofSubmitterBackoffConfig {
        self.backoff
    }
}

impl<Client> ProofSubmitter<Client>
where
    Client: ProverWorkerProvider,
{
    /// Extend a claimed proof job lock through the worker API.
    pub async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError> {
        self.client.heartbeat(request).await
    }

    /// Submits a generated proof, retrying retryable delivery failures until success.
    ///
    /// This method has no cancellation or retry limit. Use
    /// [`Self::submit_until_delivered_or_cancelled`] when submission should stop during shutdown.
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
        let attempts = Arc::new(AtomicU64::new(0));
        let request_for_submit = request.clone();
        let attempts_for_submit = Arc::clone(&attempts);
        let cancel_for_submit = cancel.clone();
        let cancel_for_retry = cancel.clone();
        let cancel_for_sleep = cancel.clone();

        let response = (|| {
            let request = request_for_submit.clone();
            let attempts = Arc::clone(&attempts_for_submit);
            let cancel = cancel_for_submit.clone();

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
        .sleep(move |delay| {
            let cancel = cancel_for_sleep.clone();

            async move {
                tokio::select! {
                    () = cancel.cancelled() => {}
                    () = sleep(delay) => {}
                }
            }
        })
        .when(move |error| match error {
            ProofSubmitterError::Submit(error) => {
                !cancel_for_retry.is_cancelled() && error.is_retryable()
            }
            ProofSubmitterError::UnsupportedProofResult | ProofSubmitterError::Cancelled => false,
        })
        .notify(|error, delay| {
            if let ProofSubmitterError::Submit(error) = error {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    attempts = attempts.load(Ordering::Relaxed),
                    backoff_ms = delay.as_millis(),
                    error = %error,
                    "proof submission failed; retrying"
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
                    attempts = attempts.load(Ordering::Relaxed),
                    "proof submission delivered"
                );
                Ok(response)
            }
            Err(ProofSubmitterError::Cancelled) => {
                info!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    attempts = attempts.load(Ordering::Relaxed),
                    "proof submission cancelled"
                );
                Err(ProofSubmitterError::Cancelled)
            }
            Err(ProofSubmitterError::Submit(error)) => {
                warn!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    attempts = attempts.load(Ordering::Relaxed),
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
                    attempts = attempts.load(Ordering::Relaxed),
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
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{B256, Bytes};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofRequest as PrimitiveProofRequest, Proposal};
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse, ProofJob,
        ProofJobStatus, ProofRequest, ProofRequestKind, TeeProofRequest,
    };
    use chrono::Utc;
    use tokio::time::{sleep, timeout};

    use super::*;

    #[derive(Clone, Debug)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
    }

    #[derive(Debug)]
    struct MockWorkerState {
        failures: Vec<ProverServiceClientError>,
        submissions: Vec<WorkerSubmitProofRequest>,
        always_retryable_failure: bool,
        response_delay: Option<Duration>,
    }

    impl MockWorkerClient {
        fn new(failures: Vec<ProverServiceClientError>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    failures,
                    submissions: Vec::new(),
                    always_retryable_failure: false,
                    response_delay: None,
                })),
            }
        }

        fn always_retryable_failure() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    failures: Vec::new(),
                    submissions: Vec::new(),
                    always_retryable_failure: true,
                    response_delay: None,
                })),
            }
        }

        fn with_response_delay(self, response_delay: Duration) -> Self {
            self.state.lock().expect("mock state poisoned").response_delay = Some(response_delay);
            self
        }

        fn submission_count(&self) -> usize {
            self.state.lock().expect("mock state poisoned").submissions.len()
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
            let response_delay = {
                let mut state = self.state.lock().expect("mock state poisoned");
                state.submissions.push(request.clone());
                if state.always_retryable_failure {
                    return Err(retryable_error());
                }
                if !state.failures.is_empty() {
                    return Err(state.failures.remove(0));
                }
                state.response_delay
            };

            if let Some(response_delay) = response_delay {
                sleep(response_delay).await;
            }

            Ok(WorkerSubmitProofResponse { job: proof_job_for_submission(&request) })
        }
    }

    fn retryable_error() -> ProverServiceClientError {
        ProverServiceClientError::Timeout("service unavailable".to_string())
    }

    fn non_retryable_error() -> ProverServiceClientError {
        ProverServiceClientError::WorkerLeaseRejected {
            message: "proof job lock is not owned by worker".to_string(),
        }
    }

    fn proposal(block: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(1),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(2),
            l1_origin_number: block.saturating_sub(1),
            l2_block_number: block,
            prev_output_root: B256::repeat_byte(3),
            config_hash: B256::repeat_byte(4),
        }
    }

    fn nitro_tee_proof() -> NitroProofResult {
        NitroProofResult::Tee {
            aggregate_proposal: proposal(10),
            proposals: vec![proposal(8), proposal(9), proposal(10)],
        }
    }

    fn submit_request() -> WorkerSubmitProofRequest {
        ProofSubmitterRequest::from_tee_proof(
            "session-1".to_string(),
            "lock-1".to_string(),
            "worker-1".to_string(),
            nitro_tee_proof(),
        )
        .expect("tee proof should build a submission request")
    }

    fn proof_job_for_submission(request: &WorkerSubmitProofRequest) -> ProofJob {
        let now = Utc::now();
        ProofJob {
            session_id: request.session_id.clone(),
            status: ProofJobStatus::Succeeded,
            request: ProofRequest {
                session_id: Some(request.session_id.clone()),
                request: ProofRequestKind::Tee(TeeProofRequest {
                    proof: PrimitiveProofRequest::default(),
                    tee_kind: TeeKind::AwsNitro,
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

    async fn wait_for_submission(client: &MockWorkerClient) {
        for _ in 0..50 {
            if client.submission_count() > 0 {
                return;
            }
            sleep(Duration::from_millis(1)).await;
        }

        panic!("expected proof submission attempt")
    }

    #[test]
    fn backoff_config_normalizes_and_builds_backon_exponential_builder() {
        let backoff =
            ProofSubmitterBackoffConfig::new(Duration::from_millis(5), Duration::from_millis(12));

        let builder = backoff.to_backoff_builder();

        assert_eq!(backoff.normalized_initial_delay(), Duration::from_millis(5));
        assert_eq!(backoff.normalized_max_delay(), Duration::from_millis(12));
        assert!(format!("{builder:?}").contains("max_times: None"));
    }

    #[test]
    fn tee_proof_request_wraps_nitro_result_for_worker_api() {
        let request = submit_request();

        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.lock_id, "lock-1");
        assert_eq!(request.worker_id, "worker-1");
        let ServiceProofResult::Tee(result) = request.result else {
            panic!("expected tee proof result");
        };
        assert_eq!(result.tee_kind, TeeKind::AwsNitro);
        assert_eq!(result.aggregate_proposal.l2_block_number, 10);
        assert_eq!(result.proposals.len(), 3);
    }

    #[test]
    fn tee_proof_request_rejects_non_tee_result() {
        let result = ProofSubmitterRequest::from_tee_proof(
            "session-1".to_string(),
            "lock-1".to_string(),
            "worker-1".to_string(),
            NitroProofResult::Zk { proof_bytes: vec![1, 2, 3] },
        );

        assert!(matches!(result, Err(ProofSubmitterError::UnsupportedProofResult)));
    }

    #[tokio::test]
    async fn submitter_retries_until_submission_is_delivered() {
        let client = MockWorkerClient::new(vec![retryable_error(), retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            ProofSubmitterBackoffConfig::new(Duration::from_millis(1), Duration::from_millis(2)),
        );

        let response = submitter
            .submit_until_delivered(submit_request())
            .await
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 3);
    }

    #[tokio::test]
    async fn submitter_stops_on_non_retryable_error() {
        let client = MockWorkerClient::new(vec![non_retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            ProofSubmitterBackoffConfig::new(Duration::from_millis(1), Duration::from_millis(2)),
        );

        let result = submitter.submit_until_delivered(submit_request()).await;

        assert!(matches!(result, Err(ProofSubmitterError::Submit(_))));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn submitter_can_run_as_spawned_task() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            ProofSubmitterBackoffConfig::new(Duration::from_millis(1), Duration::from_millis(2)),
        );

        let handle = submitter.spawn_until_delivered(submit_request(), CancellationToken::new());
        let response = handle
            .await
            .expect("submission task should not panic")
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 1);

        let handle = submitter.spawn_until_delivered(submit_request(), CancellationToken::new());
        let response = handle
            .await
            .expect("submission task should not panic")
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 2);
    }

    #[tokio::test]
    async fn spawned_submitter_stops_when_cancelled() {
        let client = MockWorkerClient::always_retryable_failure();
        let submitter = ProofSubmitter::new(client.clone()).with_backoff_config(
            ProofSubmitterBackoffConfig::new(Duration::from_secs(1), Duration::from_secs(1)),
        );
        let cancel = CancellationToken::new();

        let handle = submitter.spawn_until_delivered(submit_request(), cancel.clone());
        wait_for_submission(&client).await;

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancelled submission task should finish")
            .expect("submission task should not panic");

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn cancellation_does_not_abort_in_flight_submission() {
        let client =
            MockWorkerClient::new(Vec::new()).with_response_delay(Duration::from_millis(25));
        let submitter = ProofSubmitter::new(client.clone());
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        let handle = tokio::spawn(async move {
            submitter.submit_until_delivered_or_cancelled(submit_request(), &cancel_for_task).await
        });
        wait_for_submission(&client).await;

        cancel.cancel();
        let response = timeout(Duration::from_secs(1), handle)
            .await
            .expect("in-flight submission should finish")
            .expect("submission task should not panic")
            .expect("in-flight submission should return its response");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 1);
    }
}
