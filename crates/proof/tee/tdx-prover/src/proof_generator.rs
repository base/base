//! Proof generation orchestration for claimed TDX worker jobs.

use async_trait::async_trait;
use base_proof_host::ProverError;
use base_proof_primitives::ProofResult as PrimitiveProofResult;
use base_proof_worker::{
    ClaimedProofJobHandler, ClaimedProofJobMetadata, ClaimedProofJobMetadataError, ProofSubmitter,
    ProofTaskController, WorkerHeartbeat, WorkerHeartbeatConfig,
};
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    ProofJob, ProofRequestKind, ProofResult as ServiceProofResult, TeeKind, TeeProofResult,
    WorkerSubmitProofRequest,
};
use thiserror::Error;
use tracing::info;

use crate::{TdxBackend, TdxEnclaveService};

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

#[async_trait]
impl<Client> ClaimedProofJobHandler for ProofGenerator<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    type Error = ProofGeneratorError;

    async fn handle_claimed_job(&self, job: ProofJob) -> Result<(), Self::Error> {
        let claim = ClaimedProofJobMetadata::try_from(&job)?;
        let tee = match job.request.request {
            ProofRequestKind::Tee(tee) if tee.tee_kind == TeeKind::IntelTdx => tee,
            _ => {
                return Err(ProofGeneratorError::UnsupportedProofRequest {
                    session_id: claim.session_id,
                });
            }
        };
        let proof_request = tee.proof;

        info!(
            session_id = %claim.session_id,
            lock_id = %claim.lock_id,
            worker_id = %claim.worker_id,
            l2_block = proof_request.claimed_l2_block_number,
            "starting tdx proof generation"
        );

        let heartbeat = WorkerHeartbeat::until_failure(&self.submitter, &claim, self.heartbeat);
        let generate = self.enclave.service().prove_block(proof_request);
        tokio::pin!(generate);
        tokio::pin!(heartbeat);

        let proof = tokio::select! {
            biased;
            result = &mut generate => match result {
                Ok(result) => result,
                Err(source) => return Err(ProofGeneratorError::Generate {
                    session_id: claim.session_id.clone(),
                    source,
                }),
            },
            source = &mut heartbeat => return Err(ProofGeneratorError::Heartbeat {
                session_id: claim.session_id.clone(),
                source,
            }),
        };
        drop(heartbeat);

        let PrimitiveProofResult::Tee { aggregate_proposal, proposals } = proof else {
            unreachable!("tdx backend returned non-tee proof");
        };
        let submit_request = WorkerSubmitProofRequest {
            session_id: claim.session_id.clone(),
            lock_id: claim.lock_id.clone(),
            worker_id: claim.worker_id.clone(),
            result: ServiceProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::IntelTdx,
            }),
        };
        drop(self.tasks.spawn_submission(&self.submitter, submit_request));

        info!(
            session_id = %claim.session_id,
            lock_id = %claim.lock_id,
            worker_id = %claim.worker_id,
            "tdx proof generated; proof submitter task spawned"
        );

        Ok(())
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
}
