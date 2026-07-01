//! Proof generation orchestration for claimed TDX worker jobs.

use std::future::Future;

use async_trait::async_trait;
use base_proof_primitives::ProofResult as PrimitiveProofResult;
use base_proof_worker::{
    ClaimedProofJobHandler, ClaimedProofJobMetadata, ClaimedProofJobMetadataError,
    ProofSubmissionTask, ProofSubmitter, ProofSubmitterError, ProofTaskController, WorkerHeartbeat,
    WorkerHeartbeatConfig,
};
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    ProofJob, ProofRequestKind, ProofResult as ServiceProofResult, TeeKind, TeeProofResult,
    WorkerSubmitProofRequest,
};
use thiserror::Error;
use tracing::{info, warn};

use crate::TdxEnclaveService;

/// Boxed error returned by the TDX proof service.
pub type ProofGeneratorBoxError = Box<dyn std::error::Error + Send + Sync>;

/// Default worker identifier prefix used by TDX worker configs.
pub const DEFAULT_TDX_WORKER_ID: &str = "tdx-prover";

/// Claimed prover-service job data needed to generate and submit a TDX proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofGeneratorRequest {
    /// Common worker claim metadata.
    pub claim: ClaimedProofJobMetadata,
    /// Primitive TEE proof request.
    pub proof: base_proof_primitives::ProofRequest,
}

impl TryFrom<ProofJob> for ProofGeneratorRequest {
    type Error = ProofGeneratorError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let claim = ClaimedProofJobMetadata::from_job(&job)?;

        let ProofRequestKind::Tee(tee) = job.request.request else {
            return Err(ProofGeneratorError::UnsupportedProofRequest {
                session_id: claim.session_id,
            });
        };
        if tee.tee_kind != TeeKind::IntelTdx {
            return Err(ProofGeneratorError::UnsupportedTeeKind {
                session_id: claim.session_id,
                tee_kind: tee.tee_kind,
            });
        }

        Ok(Self { claim, proof: tee.proof })
    }
}

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct TdxProofSubmitterRequest;

impl TdxProofSubmitterRequest {
    /// Builds a worker proof submission request from a generated TDX TEE proof.
    pub fn from_tee_proof(
        session_id: String,
        lock_id: String,
        worker_id: String,
        proof: PrimitiveProofResult,
    ) -> Result<WorkerSubmitProofRequest, ProofSubmitterError> {
        let PrimitiveProofResult::Tee { aggregate_proposal, proposals } = proof else {
            return Err(ProofSubmitterError::UnsupportedProofResult);
        };

        Ok(WorkerSubmitProofRequest {
            session_id,
            lock_id,
            worker_id,
            result: ServiceProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::IntelTdx,
            }),
        })
    }
}

/// Orchestrates TDX proof generation, claim heartbeats, and async proof submission.
#[derive(Debug)]
pub struct ProofGenerator<Client> {
    enclave: TdxEnclaveService,
    submitter: ProofSubmitter<Client>,
    tasks: ProofTaskController,
    heartbeat: WorkerHeartbeatConfig,
}

impl<Client> ProofGenerator<Client> {
    /// Create a proof generator with its own submission cancellation token.
    pub fn new(
        enclave: TdxEnclaveService,
        submitter: ProofSubmitter<Client>,
        heartbeat: WorkerHeartbeatConfig,
    ) -> Self {
        Self { enclave, submitter, tasks: ProofTaskController::new(), heartbeat }
    }

    /// Returns the proof submitter.
    pub const fn submitter(&self) -> &ProofSubmitter<Client> {
        &self.submitter
    }

    /// Returns the heartbeat settings used while proofs are generated.
    pub const fn heartbeat_config(&self) -> WorkerHeartbeatConfig {
        self.heartbeat
    }
}

impl<Client> ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Generate a proof for a claimed worker job and spawn proof submission.
    pub async fn generate_and_submit(
        &self,
        job: ProofJob,
    ) -> Result<ProofSubmissionTask, ProofGeneratorError> {
        let request = ProofGeneratorRequest::try_from(job)?;

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            l2_block = request.proof.claimed_l2_block_number,
            "starting tdx proof generation"
        );

        let proof = match self
            .with_heartbeat_while_generating(&request, self.prove(request.proof.clone()))
            .await
        {
            Ok(proof) => proof,
            Err(error @ ProofGeneratorError::Generate { .. }) => {
                warn!(
                    session_id = %request.claim.session_id,
                    lock_id = %request.claim.lock_id,
                    worker_id = %request.claim.worker_id,
                    error = %error,
                    "tdx proof generation failed"
                );
                return Err(error);
            }
            Err(error @ ProofGeneratorError::Heartbeat { .. }) => {
                warn!(
                    session_id = %request.claim.session_id,
                    lock_id = %request.claim.lock_id,
                    worker_id = %request.claim.worker_id,
                    error = %error,
                    "aborting tdx proof generation due to heartbeat failure"
                );
                return Err(error);
            }
            Err(
                source @ (ProofGeneratorError::MissingLockId { .. }
                | ProofGeneratorError::MissingWorkerId { .. }
                | ProofGeneratorError::UnsupportedProofRequest { .. }
                | ProofGeneratorError::UnsupportedTeeKind { .. }
                | ProofGeneratorError::BuildSubmission { .. }),
            ) => {
                unreachable!(
                    "with_heartbeat_while_generating returned an impossible error: {source}"
                );
            }
        };

        let submit_request = TdxProofSubmitterRequest::from_tee_proof(
            request.claim.session_id.clone(),
            request.claim.lock_id.clone(),
            request.claim.worker_id.clone(),
            proof,
        )
        .map_err(|source| ProofGeneratorError::BuildSubmission {
            session_id: request.claim.session_id.clone(),
            source,
        })?;
        let submit_handle = self.tasks.spawn_submission(&self.submitter, submit_request);

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            "tdx proof generated; proof submitter task spawned"
        );

        Ok(ProofSubmissionTask::new(request.claim, submit_handle))
    }

    async fn prove(
        &self,
        request: base_proof_primitives::ProofRequest,
    ) -> Result<PrimitiveProofResult, ProofGeneratorBoxError> {
        self.enclave.service().prove_block(request).await.map_err(|error| Box::new(error).into())
    }

    async fn with_heartbeat_while_generating<Output, Generate>(
        &self,
        request: &ProofGeneratorRequest,
        generate: Generate,
    ) -> Result<Output, ProofGeneratorError>
    where
        Generate: Future<Output = Result<Output, ProofGeneratorBoxError>>,
    {
        let heartbeat =
            WorkerHeartbeat::until_failure(&self.submitter, &request.claim, self.heartbeat);
        tokio::pin!(generate);
        tokio::pin!(heartbeat);

        tokio::select! {
            biased;
            result = &mut generate => result.map_err(|source| ProofGeneratorError::Generate {
                session_id: request.claim.session_id.clone(),
                source,
            }),
            source = &mut heartbeat => {
                match generate.await {
                    Ok(_) => {
                        info!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            l2_block = request.proof.claimed_l2_block_number,
                            "discarding tdx proof generated after heartbeat failure"
                        );
                    }
                    Err(error) => {
                        warn!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            error = %error,
                            "tdx proof generation finished with error after heartbeat failure"
                        );
                    }
                }

                Err(ProofGeneratorError::Heartbeat {
                    session_id: request.claim.session_id.clone(),
                    source,
                })
            },
        }
    }
}

#[async_trait]
impl<Client> ClaimedProofJobHandler for ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    type Error = ProofGeneratorError;

    async fn handle_claimed_job(&self, job: ProofJob) -> Result<(), Self::Error> {
        Self::generate_and_submit(self, job).await.map(drop)
    }

    fn shutdown(&self) {
        self.tasks.cancel_submissions();
    }
}

/// Errors raised while generating and dispatching TDX proof submissions.
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
    #[error("proof job {session_id} is not a TEE proof request")]
    UnsupportedProofRequest {
        /// Proof session identifier.
        session_id: String,
    },
    /// Claimed TEE proof job is not for Intel TDX.
    #[error("proof job {session_id} is not an Intel TDX proof request: got {tee_kind:?}")]
    UnsupportedTeeKind {
        /// Proof session identifier.
        session_id: String,
        /// TEE kind from the claimed proof job.
        tee_kind: TeeKind,
    },
    /// TDX proof generation failed.
    #[error("proof generation failed for job {session_id}: {source}")]
    Generate {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof generation error.
        #[source]
        source: ProofGeneratorBoxError,
    },
    /// Worker API heartbeat failed while the proof was being generated.
    #[error("heartbeat failed while generating proof for job {session_id}: {source}")]
    Heartbeat {
        /// Proof session identifier.
        session_id: String,
        /// Underlying worker API error.
        #[source]
        source: ProverServiceClientError,
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

impl From<ClaimedProofJobMetadataError> for ProofGeneratorError {
    fn from(error: ClaimedProofJobMetadataError) -> Self {
        match error {
            ClaimedProofJobMetadataError::MissingLockId { session_id } => {
                Self::MissingLockId { session_id }
            }
            ClaimedProofJobMetadataError::MissingWorkerId { session_id } => {
                Self::MissingWorkerId { session_id }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes};
    use base_proof_primitives::Proposal;
    use base_prover_service_protocol::{
        ProofJobStatus, ProofRequest, TeeProofRequest, WorkerSubmitProofRequest,
    };
    use chrono::Utc;

    use super::*;

    fn proof_job(tee_kind: TeeKind) -> ProofJob {
        let now = Utc::now();
        ProofJob {
            session_id: "session-1".to_owned(),
            status: ProofJobStatus::Claimed,
            request: ProofRequest {
                session_id: "session-1".to_owned(),
                request: ProofRequestKind::Tee(TeeProofRequest {
                    proof: base_proof_primitives::ProofRequest::default(),
                    tee_kind,
                }),
            },
            attempt: 1,
            lock_id: Some("lock-1".to_owned()),
            worker_id: Some("worker-1".to_owned()),
            lock_expires_at: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            error_message: None,
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

    #[test]
    fn request_accepts_intel_tdx_jobs() {
        let request = ProofGeneratorRequest::try_from(proof_job(TeeKind::IntelTdx)).unwrap();

        assert_eq!(request.claim.session_id, "session-1");
    }

    #[test]
    fn request_rejects_other_tee_kinds() {
        let err = ProofGeneratorRequest::try_from(proof_job(TeeKind::AwsNitro)).unwrap_err();

        assert!(matches!(err, ProofGeneratorError::UnsupportedTeeKind { .. }));
    }

    #[test]
    fn submit_request_marks_result_as_intel_tdx() {
        let submit = TdxProofSubmitterRequest::from_tee_proof(
            "session-1".to_owned(),
            "lock-1".to_owned(),
            "worker-1".to_owned(),
            PrimitiveProofResult::Tee {
                aggregate_proposal: proposal(10),
                proposals: vec![proposal(10)],
            },
        )
        .unwrap();

        let WorkerSubmitProofRequest { result: ServiceProofResult::Tee(result), .. } = submit
        else {
            panic!("expected TEE result");
        };
        assert_eq!(result.tee_kind, TeeKind::IntelTdx);
    }
}
