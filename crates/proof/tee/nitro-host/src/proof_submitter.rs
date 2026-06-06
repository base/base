//! Async proof submission task for prover-service worker delivery.

use base_proof_primitives::ProofResult as NitroProofResult;
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    HeartbeatRequest, HeartbeatResponse, ProofResult as ServiceProofResult, TeeKind,
    TeeProofResult, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Errors raised while preparing or submitting a generated proof.
#[derive(Debug, Error)]
pub enum ProofSubmitterError {
    /// Nitro proof submitter only submits TEE proof results.
    #[error("nitro proof submitter only accepts TEE proof results")]
    UnsupportedProofResult,
    /// Proof submission was cancelled before it started.
    #[error("proof submission cancelled before it started")]
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
}

impl<Client> ProofSubmitter<Client> {
    /// Creates a proof submitter.
    pub const fn new(client: Client) -> Self {
        Self { client }
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

    /// Submits a generated proof through the worker client.
    pub async fn submit(
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
}

impl<Client> ProofSubmitter<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Spawns proof submission as an async Tokio task.
    pub fn spawn_submit(
        &self,
        request: WorkerSubmitProofRequest,
        cancel: CancellationToken,
    ) -> JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>> {
        let submitter = self.clone();
        tokio::spawn(async move {
            if cancel.is_cancelled() {
                info!(
                    session_id = %request.session_id,
                    lock_id = %request.lock_id,
                    worker_id = %request.worker_id,
                    "proof submission cancelled"
                );
                return Err(ProofSubmitterError::Cancelled);
            }

            // After this point, cancellation is treated as in-flight and the
            // submission RPC is allowed to finish.
            submitter.submit(request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

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
        response_delay: Option<Duration>,
    }

    impl MockWorkerClient {
        fn new(failures: Vec<ProverServiceClientError>) -> Self {
            Self {
                state: Arc::new(Mutex::new(MockWorkerState {
                    failures,
                    submissions: Vec::new(),
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
                session_id: request.session_id.clone(),
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
    async fn submitter_does_not_retry_worker_client_error() {
        let client = MockWorkerClient::new(vec![retryable_error(), retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone());

        let result = submitter.submit(submit_request()).await;

        assert!(matches!(result, Err(ProofSubmitterError::Submit(_))));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn submitter_stops_on_non_retryable_error() {
        let client = MockWorkerClient::new(vec![non_retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone());

        let result = submitter.submit(submit_request()).await;

        assert!(matches!(result, Err(ProofSubmitterError::Submit(_))));
        assert_eq!(client.submission_count(), 1);
    }

    #[tokio::test]
    async fn submitter_can_run_as_spawned_task() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());

        let handle = submitter.spawn_submit(submit_request(), CancellationToken::new());
        let response = handle
            .await
            .expect("submission task should not panic")
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 1);

        let handle = submitter.spawn_submit(submit_request(), CancellationToken::new());
        let response = handle
            .await
            .expect("submission task should not panic")
            .expect("submission should eventually succeed");

        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        assert_eq!(client.submission_count(), 2);
    }

    #[tokio::test]
    async fn spawned_submitter_stops_when_cancelled_before_submission() {
        let client = MockWorkerClient::new(Vec::new());
        let submitter = ProofSubmitter::new(client.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let handle = submitter.spawn_submit(submit_request(), cancel.clone());
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancelled submission task should finish")
            .expect("submission task should not panic");

        assert!(matches!(result, Err(ProofSubmitterError::Cancelled)));
        assert_eq!(client.submission_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_does_not_abort_in_flight_submission() {
        let client =
            MockWorkerClient::new(Vec::new()).with_response_delay(Duration::from_millis(25));
        let submitter = ProofSubmitter::new(client.clone());
        let cancel = CancellationToken::new();

        let handle = submitter.spawn_submit(submit_request(), cancel.clone());
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
