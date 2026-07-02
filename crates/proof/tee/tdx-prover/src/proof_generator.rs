//! Proof generation orchestration for claimed TDX worker jobs.

use std::future::Future;

use async_trait::async_trait;
use base_proof_host::ProverError;
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

use crate::{TdxBackend, TdxEnclaveService};

/// Default worker identifier prefix used by TDX worker configs.
pub const DEFAULT_TDX_WORKER_ID: &str = "tdx-prover";

/// Claimed prover-service job data needed to generate and submit a TDX proof.
#[derive(Debug)]
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

        let proof = self
            .with_heartbeat_while_generating(
                &request,
                self.enclave.service().prove_block(request.proof.clone()),
            )
            .await?;

        let PrimitiveProofResult::Tee { aggregate_proposal, proposals } = proof else {
            return Err(ProofGeneratorError::BuildSubmission {
                session_id: request.claim.session_id.clone(),
                source: ProofSubmitterError::UnsupportedProofResult,
            });
        };
        let submit_request = WorkerSubmitProofRequest {
            session_id: request.claim.session_id.clone(),
            lock_id: request.claim.lock_id.clone(),
            worker_id: request.claim.worker_id.clone(),
            result: ServiceProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::IntelTdx,
            }),
        };
        let submit_handle = self.tasks.spawn_submission(&self.submitter, submit_request);

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            "tdx proof generated; proof submitter task spawned"
        );

        Ok(ProofSubmissionTask::new(request.claim, submit_handle))
    }

    async fn with_heartbeat_while_generating(
        &self,
        request: &ProofGeneratorRequest,
        generate: impl Future<Output = Result<PrimitiveProofResult, ProverError<TdxBackend>>>,
    ) -> Result<PrimitiveProofResult, ProofGeneratorError> {
        let heartbeat =
            WorkerHeartbeat::until_failure(&self.submitter, &request.claim, self.heartbeat);
        tokio::pin!(generate);
        tokio::pin!(heartbeat);

        tokio::select! {
            biased;
            result = &mut generate => match result {
                Ok(result) => Ok(result),
                Err(source) => {
                    warn!(
                        session_id = %request.claim.session_id,
                        lock_id = %request.claim.lock_id,
                        worker_id = %request.claim.worker_id,
                        error = %source,
                        "tdx proof generation failed"
                    );

                    Err(ProofGeneratorError::Generate {
                        session_id: request.claim.session_id.clone(),
                        source,
                    })
                }
            },
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

                warn!(
                    session_id = %request.claim.session_id,
                    lock_id = %request.claim.lock_id,
                    worker_id = %request.claim.worker_id,
                    error = %source,
                    "aborting tdx proof generation due to heartbeat failure"
                );

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
    /// Claimed proof job metadata is invalid.
    #[error(transparent)]
    ClaimMetadata(#[from] ClaimedProofJobMetadataError),
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
        source: ProverError<TdxBackend>,
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

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{ProofJobStatus, ProofRequest, TeeProofRequest};
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
}
