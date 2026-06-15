//! Proof submission types for prover-service worker delivery.
//!
//! [`ProofSubmitter`] is backend-neutral: hosts build a
//! `WorkerSubmitProofRequest` from their own proof result type, then hand the
//! request to this shared worker component for delivery.

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    HeartbeatRequest, HeartbeatResponse, WorkerSubmitProofRequest, WorkerSubmitProofResponse,
};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Errors raised while preparing or submitting a generated proof.
#[derive(Debug, Error)]
pub enum ProofSubmitterError {
    /// The generated proof result is not one this worker can submit.
    #[error("proof submitter received an unsupported proof result")]
    UnsupportedProofResult,
    /// Prover service worker API submission failed.
    #[error(transparent)]
    Submit(#[from] ProverServiceClientError),
}

impl ProofSubmitterError {
    /// Returns `true` when retrying the submission may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::UnsupportedProofResult => false,
            Self::Submit(error) => error.is_retryable(),
        }
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
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
        ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult,
        RecordProofSessionRequest, RecordProofSessionResponse, ZkProofRequest, ZkProofResult, ZkVm,
    };
    use chrono::Utc;

    use super::*;

    #[derive(Clone, Debug)]
    struct MockWorkerClient {
        state: Arc<Mutex<MockWorkerState>>,
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
            }
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
            let mut state = self.state.lock().expect("mock state poisoned");
            state.submissions.push(request.clone());
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

    #[tokio::test]
    async fn submitter_returns_submission_error() {
        let client = MockWorkerClient::new(vec![retryable_error()]);
        let submitter = ProofSubmitter::new(client.clone());

        let result = submitter.submit_once(submit_request()).await;

        assert!(matches!(result, Err(ProofSubmitterError::Submit(_))));
        assert_eq!(client.submission_count(), 1);
    }
}
