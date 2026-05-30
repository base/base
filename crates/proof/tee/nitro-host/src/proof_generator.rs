//! Proof generation orchestration for claimed Nitro worker jobs.

use std::sync::Arc;

use async_trait::async_trait;
use base_proof_primitives::{ProofRequest as NitroProofRequest, ProofResult as NitroProofResult};
use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::{ProofJob, ProofRequestKind, WorkerSubmitProofResponse};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    NitroEnclavePool, NitroEnclavePoolError, ProofSubmitter, ProofSubmitterError,
    ProofSubmitterRequest,
};

/// Executes Nitro proof generation from a primitive proof request.
#[async_trait]
pub trait ProofProducer: Send + Sync {
    /// Generate a proof for the requested block range.
    async fn prove(
        &self,
        request: NitroProofRequest,
    ) -> Result<NitroProofResult, NitroEnclavePoolError>;
}

#[async_trait]
impl ProofProducer for NitroEnclavePool {
    async fn prove(
        &self,
        request: NitroProofRequest,
    ) -> Result<NitroProofResult, NitroEnclavePoolError> {
        Self::prove(self, request).await
    }
}

/// Claimed prover-service job data needed to generate and submit a Nitro proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofGeneratorRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Primitive Nitro proof request.
    pub proof: NitroProofRequest,
}

impl TryFrom<ProofJob> for ProofGeneratorRequest {
    type Error = ProofGeneratorError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let session_id = job.session_id;
        let lock_id = job
            .lock_id
            .ok_or_else(|| ProofGeneratorError::MissingLockId { session_id: session_id.clone() })?;
        let worker_id = job.worker_id.ok_or_else(|| ProofGeneratorError::MissingWorkerId {
            session_id: session_id.clone(),
        })?;

        let ProofRequestKind::Tee(tee) = job.request.request else {
            return Err(ProofGeneratorError::UnsupportedProofRequest { session_id });
        };

        Ok(Self { session_id, lock_id, worker_id, proof: tee.proof })
    }
}

/// Handle for a proof submission task spawned after successful proof generation.
#[derive(Debug)]
pub struct ProofGeneratorTask {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Spawned proof submission task.
    pub submit_handle: JoinHandle<Result<WorkerSubmitProofResponse, ProofSubmitterError>>,
}

/// Orchestrates Nitro witness generation, enclave proving, and async proof submission.
#[derive(Debug)]
pub struct ProofGenerator<Producer, Client> {
    producer: Arc<Producer>,
    submitter: ProofSubmitter<Client>,
    submission_cancel: CancellationToken,
}

impl<Producer, Client> ProofGenerator<Producer, Client> {
    /// Create a proof generator with its own submission cancellation token.
    pub fn new(producer: Arc<Producer>, submitter: ProofSubmitter<Client>) -> Self {
        Self { producer, submitter, submission_cancel: CancellationToken::new() }
    }

    /// Use a caller-provided cancellation token for spawned submission tasks.
    pub fn with_submission_cancel(mut self, submission_cancel: CancellationToken) -> Self {
        self.submission_cancel = submission_cancel;
        self
    }

    /// Returns the proof producer.
    pub fn producer(&self) -> Arc<Producer> {
        Arc::clone(&self.producer)
    }

    /// Returns the proof submitter.
    pub const fn submitter(&self) -> &ProofSubmitter<Client> {
        &self.submitter
    }

    /// Returns the cancellation token used for spawned submission tasks.
    pub fn submission_cancel(&self) -> CancellationToken {
        self.submission_cancel.clone()
    }
}

impl<Producer, Client> ProofGenerator<Producer, Client>
where
    Producer: ProofProducer + 'static,
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Generate a proof for a claimed worker job and spawn proof submission.
    pub async fn generate_and_submit(
        &self,
        job: ProofJob,
    ) -> Result<ProofGeneratorTask, ProofGeneratorError> {
        let request = ProofGeneratorRequest::try_from(job)?;

        info!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            l2_block = request.proof.claimed_l2_block_number,
            "starting nitro proof generation"
        );

        let proof = self.producer.prove(request.proof).await.map_err(|source| {
            ProofGeneratorError::Generate { session_id: request.session_id.clone(), source }
        })?;

        let submit_request = ProofSubmitterRequest::from_tee_proof(
            request.session_id.clone(),
            request.lock_id.clone(),
            request.worker_id.clone(),
            proof,
        )
        .map_err(|source| ProofGeneratorError::BuildSubmission {
            session_id: request.session_id.clone(),
            source,
        })?;

        let submit_handle =
            self.submitter.spawn_until_delivered(submit_request, self.submission_cancel.clone());

        info!(
            session_id = %request.session_id,
            lock_id = %request.lock_id,
            worker_id = %request.worker_id,
            "nitro proof generated; proof submitter task spawned"
        );

        Ok(ProofGeneratorTask {
            session_id: request.session_id,
            lock_id: request.lock_id,
            worker_id: request.worker_id,
            submit_handle,
        })
    }
}

/// Errors raised while generating and dispatching Nitro proof submissions.
#[derive(Debug, Error)]
pub enum ProofGeneratorError {
    /// Claimed proof job did not include a lock identifier.
    #[error("proof job {session_id} is missing lock_id")]
    MissingLockId {
        /// Proof session identifier.
        session_id: String,
    },
    /// Claimed proof job did not include a worker identifier.
    #[error("proof job {session_id} is missing worker_id")]
    MissingWorkerId {
        /// Proof session identifier.
        session_id: String,
    },
    /// Claimed proof job is not a TEE proof request.
    #[error("proof job {session_id} is not an AWS Nitro TEE proof request")]
    UnsupportedProofRequest {
        /// Proof session identifier.
        session_id: String,
    },
    /// Witness generation or enclave proving failed.
    #[error("proof generation failed for job {session_id}: {source}")]
    Generate {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof generation error.
        #[source]
        source: NitroEnclavePoolError,
    },
    /// The generated proof could not be converted into a worker submission request.
    #[error("failed to build proof submission for job {session_id}: {source}")]
    BuildSubmission {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof submission request error.
        #[source]
        source: ProofSubmitterError,
    },
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use alloy_primitives::{B256, Bytes};
    use base_proof_primitives::Proposal;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        GetNextProofRequest, GetNextProofResponse, HeartbeatRequest, HeartbeatResponse,
        ProofJobStatus, ProofRequest, ProofResult as ServiceProofResult, TeeKind, TeeProofRequest,
        WorkerSubmitProofRequest,
    };
    use chrono::Utc;
    use tokio::time::timeout;

    use super::*;

    #[derive(Debug)]
    struct MockProducer {
        result: Mutex<Option<Result<NitroProofResult, NitroEnclavePoolError>>>,
        requests: Mutex<Vec<NitroProofRequest>>,
    }

    impl MockProducer {
        fn success() -> Self {
            Self {
                result: Mutex::new(Some(Ok(nitro_tee_proof()))),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn failing(error: NitroEnclavePoolError) -> Self {
            Self { result: Mutex::new(Some(Err(error))), requests: Mutex::new(Vec::new()) }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("requests lock should not be poisoned").len()
        }
    }

    #[async_trait]
    impl ProofProducer for MockProducer {
        async fn prove(
            &self,
            request: NitroProofRequest,
        ) -> Result<NitroProofResult, NitroEnclavePoolError> {
            self.requests.lock().expect("requests lock should not be poisoned").push(request);
            self.result
                .lock()
                .expect("result lock should not be poisoned")
                .take()
                .expect("mock producer result should be configured")
        }
    }

    #[derive(Clone, Debug, Default)]
    struct MockWorkerClient {
        submissions: Arc<Mutex<Vec<WorkerSubmitProofRequest>>>,
    }

    impl MockWorkerClient {
        fn submissions(&self) -> Vec<WorkerSubmitProofRequest> {
            self.submissions.lock().expect("submissions lock should not be poisoned").clone()
        }
    }

    #[async_trait]
    impl ProverWorkerProvider for MockWorkerClient {
        async fn get_next_proof(
            &self,
            _request: GetNextProofRequest,
        ) -> Result<GetNextProofResponse, ProverServiceClientError> {
            panic!("get_next_proof is not used by proof generator tests")
        }

        async fn heartbeat(
            &self,
            _request: HeartbeatRequest,
        ) -> Result<HeartbeatResponse, ProverServiceClientError> {
            panic!("heartbeat is not used by proof generator tests")
        }

        async fn submit_proof(
            &self,
            request: WorkerSubmitProofRequest,
        ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
            self.submissions
                .lock()
                .expect("submissions lock should not be poisoned")
                .push(request.clone());

            Ok(WorkerSubmitProofResponse {
                job: proof_job(
                    request.session_id,
                    ProofJobStatus::Succeeded,
                    Some(request.lock_id),
                    Some(request.worker_id),
                    PrimitiveRequestKind::Tee,
                ),
            })
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum PrimitiveRequestKind {
        Tee,
        Compressed,
    }

    fn primitive_request(block: u64) -> NitroProofRequest {
        NitroProofRequest { claimed_l2_block_number: block, ..Default::default() }
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

    fn proof_job(
        session_id: impl Into<String>,
        status: ProofJobStatus,
        lock_id: Option<String>,
        worker_id: Option<String>,
        kind: PrimitiveRequestKind,
    ) -> ProofJob {
        let session_id = session_id.into();
        let now = Utc::now();
        let request = match kind {
            PrimitiveRequestKind::Tee => ProofRequestKind::Tee(TeeProofRequest {
                proof: primitive_request(42),
                tee_kind: TeeKind::AwsNitro,
            }),
            PrimitiveRequestKind::Compressed => {
                ProofRequestKind::Compressed(base_prover_service_protocol::ZkProofRequest {
                    start_block_number: 1,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: None,
                    intermediate_root_interval: None,
                    zk_vm: base_prover_service_protocol::ZkVm::Sp1,
                })
            }
        };

        ProofJob {
            session_id: session_id.clone(),
            status,
            request: ProofRequest { session_id: Some(session_id), request },
            attempt: 1,
            lock_id,
            worker_id,
            lock_expires_at: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            error_message: None,
        }
    }

    #[test]
    fn request_requires_claim_metadata() {
        let job = proof_job(
            "session-1",
            ProofJobStatus::Claimed,
            None,
            Some("worker-1".to_owned()),
            PrimitiveRequestKind::Tee,
        );

        let err = ProofGeneratorRequest::try_from(job).unwrap_err();

        assert!(matches!(err, ProofGeneratorError::MissingLockId { .. }));
    }

    #[test]
    fn request_rejects_non_tee_jobs() {
        let job = proof_job(
            "session-1",
            ProofJobStatus::Claimed,
            Some("lock-1".to_owned()),
            Some("worker-1".to_owned()),
            PrimitiveRequestKind::Compressed,
        );

        let err = ProofGeneratorRequest::try_from(job).unwrap_err();

        assert!(matches!(err, ProofGeneratorError::UnsupportedProofRequest { .. }));
    }

    #[tokio::test]
    async fn generate_and_submit_spawns_submitter_after_successful_proof() {
        let producer = Arc::new(MockProducer::success());
        let client = MockWorkerClient::default();
        let generator =
            ProofGenerator::new(Arc::clone(&producer), ProofSubmitter::new(client.clone()));
        let job = proof_job(
            "session-1",
            ProofJobStatus::Claimed,
            Some("lock-1".to_owned()),
            Some("worker-1".to_owned()),
            PrimitiveRequestKind::Tee,
        );

        let task = generator.generate_and_submit(job).await.expect("proof should generate");
        let response = timeout(Duration::from_secs(1), task.submit_handle)
            .await
            .expect("submit task should complete")
            .expect("submit task should not panic")
            .expect("submission should succeed");

        assert_eq!(producer.request_count(), 1);
        assert_eq!(response.job.status, ProofJobStatus::Succeeded);
        let submissions = client.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].session_id, "session-1");
        assert_eq!(submissions[0].lock_id, "lock-1");
        assert_eq!(submissions[0].worker_id, "worker-1");
        assert!(matches!(submissions[0].result, ServiceProofResult::Tee(_)));
    }

    #[tokio::test]
    async fn generate_failure_does_not_spawn_submitter() {
        let producer = Arc::new(MockProducer::failing(NitroEnclavePoolError::Busy));
        let client = MockWorkerClient::default();
        let generator =
            ProofGenerator::new(Arc::clone(&producer), ProofSubmitter::new(client.clone()));
        let job = proof_job(
            "session-1",
            ProofJobStatus::Claimed,
            Some("lock-1".to_owned()),
            Some("worker-1".to_owned()),
            PrimitiveRequestKind::Tee,
        );

        let err = generator.generate_and_submit(job).await.unwrap_err();

        assert!(matches!(err, ProofGeneratorError::Generate { .. }));
        assert_eq!(producer.request_count(), 1);
        assert!(client.submissions().is_empty());
    }
}
