//! Proof generation orchestration for claimed ZK worker jobs.

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use base_proof_worker::{
    ClaimedProofJobHandler, ClaimedProofJobMetadata, ClaimedProofJobMetadataError,
    ProofSubmissionTask, ProofSubmitter, ProofSubmitterError, ProofTaskController, WorkerHeartbeat,
};
pub use base_proof_worker::{
    DEFAULT_WORKER_HEARTBEAT_INTERVAL as DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS as DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_WORKER_MAX_CONSECUTIVE_HEARTBEAT_FAILURES as DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
    MIN_WORKER_HEARTBEAT_INTERVAL as MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    WorkerHeartbeatConfig as ProofGeneratorHeartbeatConfig,
};
use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    BackendSession, BackendSessionState, ProofJob, ProofRequestKind, ProofResult, SessionType,
    WorkerSubmitProofRequest, ZkBackend,
};
use thiserror::Error;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    ProofSessionHandle, ProofSubmitterRequest, ZkProofRequestKind, ZkProver, ZkProverError,
    ZkSessionState,
};

/// Minimum delay between backend session polls.
pub const MIN_PROOF_GENERATOR_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Default delay between backend session polls while waiting for a proof.
pub const DEFAULT_PROOF_GENERATOR_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time to wait for generation cleanup after heartbeat failure.
pub const DEFAULT_PROOF_GENERATOR_HEARTBEAT_FAILURE_DRAIN_TIMEOUT: Duration =
    Duration::from_secs(60);

/// Claimed prover-service job data needed to generate and submit a ZK proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofGeneratorRequest {
    /// Common worker claim metadata.
    pub claim: ClaimedProofJobMetadata,
    /// Concrete ZK proof request.
    pub request: ZkProofRequestKind,
}

impl TryFrom<ProofJob> for ProofGeneratorRequest {
    type Error = ProofGeneratorError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let claim = ClaimedProofJobMetadata::from_job(&job)?;

        let request = match job.request.request {
            ProofRequestKind::Compressed(request) => ZkProofRequestKind::Compressed(request),
            ProofRequestKind::SnarkGroth16(request) => ZkProofRequestKind::SnarkGroth16(request),
            ProofRequestKind::Tee(_) => {
                return Err(ProofGeneratorError::UnsupportedProofRequest {
                    session_id: claim.session_id,
                });
            }
        };

        Ok(Self { claim, request })
    }
}

/// Orchestrates ZK proof generation, claim heartbeats, and async proof submission.
#[derive(Debug)]
pub struct ProofGenerator<Client> {
    provers: HashMap<ZkBackend, Arc<dyn ZkProver>>,
    submitter: ProofSubmitter<Client>,
    tasks: ProofTaskController,
    heartbeat: ProofGeneratorHeartbeatConfig,
    poll_interval: Duration,
}

impl<Client> ProofGenerator<Client> {
    /// Create a proof generator with its own submission cancellation token.
    pub fn new(
        provers: HashMap<ZkBackend, Arc<dyn ZkProver>>,
        submitter: ProofSubmitter<Client>,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        Self {
            provers,
            submitter,
            tasks: ProofTaskController::new(),
            heartbeat,
            poll_interval: DEFAULT_PROOF_GENERATOR_POLL_INTERVAL,
        }
    }

    /// Use a caller-provided cancellation token for spawned submission tasks.
    #[must_use]
    pub fn with_submission_cancel(mut self, submission_cancel: CancellationToken) -> Self {
        self.tasks = self.tasks.with_submission_cancel(submission_cancel);
        self
    }

    /// Sets the delay between backend session polls while waiting for a proof.
    #[must_use]
    pub const fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Returns the proof submitter.
    pub const fn submitter(&self) -> &ProofSubmitter<Client> {
        &self.submitter
    }

    /// Returns the cancellation token used for spawned submission tasks.
    pub const fn submission_cancel(&self) -> &CancellationToken {
        self.tasks.submission_cancel()
    }

    /// Returns the heartbeat settings used while proofs are generated.
    pub const fn heartbeat_config(&self) -> ProofGeneratorHeartbeatConfig {
        self.heartbeat
    }

    /// Returns the backend session poll interval, clamped to the minimum allowed delay.
    pub fn normalized_poll_interval(&self) -> Duration {
        self.poll_interval.max(MIN_PROOF_GENERATOR_POLL_INTERVAL)
    }

    fn prover_for(
        &self,
        request: &ZkProofRequestKind,
    ) -> Result<&Arc<dyn ZkProver>, ZkProverError> {
        let backend = request.zk_backend();
        self.provers.get(&backend).ok_or(ZkProverError::UnsupportedBackend { backend })
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
            start_block = request.request.start_block_number(),
            block_count = request.request.number_of_blocks_to_prove(),
            zk_backend = %request.request.zk_backend(),
            "starting zk proof generation"
        );

        let result = match self
            .with_heartbeat_while_generating(&request, self.prove_to_completion(&request))
            .await
        {
            Ok(result) => result,
            Err(ProofGeneratorError::Generate { session_id, source }) => {
                warn!(
                    session_id = %request.claim.session_id,
                    lock_id = %request.claim.lock_id,
                    worker_id = %request.claim.worker_id,
                    zk_backend = %request.request.zk_backend(),
                    error = %source,
                    "zk proof generation failed"
                );

                return Err(ProofGeneratorError::Generate { session_id, source });
            }
            Err(ProofGeneratorError::Heartbeat { session_id, source }) => {
                warn!(
                    session_id = %request.claim.session_id,
                    lock_id = %request.claim.lock_id,
                    worker_id = %request.claim.worker_id,
                    zk_backend = %request.request.zk_backend(),
                    error = %source,
                    "aborting zk proof generation due to heartbeat failure"
                );

                return Err(ProofGeneratorError::Heartbeat { session_id, source });
            }
            Err(
                source @ (ProofGeneratorError::MissingLockId { .. }
                | ProofGeneratorError::MissingWorkerId { .. }
                | ProofGeneratorError::UnsupportedProofRequest { .. }
                | ProofGeneratorError::BuildSubmission { .. }),
            ) => {
                unreachable!(
                    "with_heartbeat_while_generating returned an impossible error: {source}"
                );
            }
        };

        let submit_request = WorkerSubmitProofRequest::try_from(ProofSubmitterRequest {
            session_id: request.claim.session_id.clone(),
            lock_id: request.claim.lock_id.clone(),
            worker_id: request.claim.worker_id.clone(),
            result,
        })
        .map_err(|source| ProofGeneratorError::BuildSubmission {
            session_id: request.claim.session_id.clone(),
            source,
        })?;

        let submit_handle = self.tasks.spawn_submission(&self.submitter, submit_request);

        info!(
            session_id = %request.claim.session_id,
            lock_id = %request.claim.lock_id,
            worker_id = %request.claim.worker_id,
            "zk proof generated; proof submitter task spawned"
        );

        Ok(ProofSubmissionTask::new(request.claim, submit_handle))
    }

    async fn prove_to_completion(
        &self,
        request: &ProofGeneratorRequest,
    ) -> Result<ProofResult, ZkProverError> {
        let prover = self.prover_for(&request.request)?;

        let handle = ProofSessionHandle::new(
            self.submitter.client().clone(),
            request.claim.session_id.clone(),
            request.claim.lock_id.clone(),
            request.claim.worker_id.clone(),
        );

        // Every request begins with a range (STARK) proof. For a Groth16 job that is the compressed
        // request nested in its SNARK request.
        let range_request = match &request.request {
            ZkProofRequestKind::Compressed(proof) => proof,
            ZkProofRequestKind::SnarkGroth16(snark) => &snark.proof,
        };
        let range_session_id = self
            .drive_stage(
                request,
                &handle,
                SessionType::Stark,
                prover,
                prover.submit(range_request, &request.claim.session_id),
            )
            .await?;

        // Groth16 requests aggregate the completed range proof into a SNARK; every other request
        // downloads the range proof directly.
        match &request.request {
            ZkProofRequestKind::SnarkGroth16(proof_request) => {
                let snark_session_id = self
                    .drive_stage(
                        request,
                        &handle,
                        SessionType::Snark,
                        prover,
                        prover.submit_next(
                            proof_request,
                            &request.claim.session_id,
                            &range_session_id,
                        ),
                    )
                    .await?;
                prover.download(SessionType::Snark, &snark_session_id).await
            }
            ZkProofRequestKind::Compressed(_) => {
                prover.download(SessionType::Stark, &range_session_id).await
            }
        }
    }

    /// Drive one proving stage to completion, resuming any session recorded on a previous run.
    ///
    /// `submit` is awaited only when there is no reusable session. A session reported as already
    /// completed is returned as-is: the backend may have purged the finished proof, so re-polling
    /// could trigger a spurious resubmission of an already-finished stage.
    async fn drive_stage(
        &self,
        request: &ProofGeneratorRequest,
        handle: &ProofSessionHandle<Client>,
        session_type: SessionType,
        prover: &Arc<dyn ZkProver>,
        submit: impl Future<Output = Result<String, ZkProverError>>,
    ) -> Result<String, ZkProverError> {
        let backend_session_id = match handle
            .get(session_type)
            .await
            .map_err(|error| ZkProverError::Session(Box::new(error)))?
        {
            Some(BackendSession { backend_session_id, state: BackendSessionState::Completed }) => {
                return Ok(backend_session_id);
            }
            Some(BackendSession { backend_session_id, state: BackendSessionState::Running }) => {
                info!(
                    session_id = %request.claim.session_id,
                    backend_session_id = %backend_session_id,
                    ?session_type,
                    "resuming in-flight backend session"
                );
                backend_session_id
            }
            None
            | Some(BackendSession {
                state: BackendSessionState::Submitting | BackendSessionState::Failed,
                ..
            }) => {
                let backend_session_id = submit.await?;
                handle
                    .record(session_type, backend_session_id.clone(), BackendSessionState::Running)
                    .await
                    .map_err(|error| ZkProverError::Session(Box::new(error)))?;
                info!(
                    session_id = %request.claim.session_id,
                    backend_session_id = %backend_session_id,
                    ?session_type,
                    "submitted backend session and recorded it"
                );
                backend_session_id
            }
        };

        self.poll_to_completion(request, handle, session_type, prover, backend_session_id).await
    }

    /// Poll a running backend session until it reaches a terminal state.
    async fn poll_to_completion(
        &self,
        request: &ProofGeneratorRequest,
        handle: &ProofSessionHandle<Client>,
        session_type: SessionType,
        prover: &Arc<dyn ZkProver>,
        backend_session_id: String,
    ) -> Result<String, ZkProverError> {
        loop {
            match prover.poll(&backend_session_id).await? {
                ZkSessionState::Running => {
                    debug!(
                        session_id = %request.claim.session_id,
                        backend_session_id = %backend_session_id,
                        ?session_type,
                        "backend session still running"
                    );
                    sleep(self.normalized_poll_interval()).await;
                }
                ZkSessionState::Completed => {
                    // The worker API rejects terminal session states, so completion is not
                    // recorded; the stage resolves by returning its backend session id.
                    debug!(
                        session_id = %request.claim.session_id,
                        backend_session_id = %backend_session_id,
                        ?session_type,
                        "backend session completed"
                    );
                    return Ok(backend_session_id);
                }
                ZkSessionState::Failed(reason) => {
                    warn!(
                        session_id = %request.claim.session_id,
                        backend_session_id = %backend_session_id,
                        ?session_type,
                        error = %reason,
                        "backend session failed; marking for resubmission"
                    );
                    self.record_backend_resubmission(
                        request,
                        handle,
                        session_type,
                        &backend_session_id,
                    )
                    .await;
                    return Err(ZkProverError::BackendSessionFailed { backend_session_id, reason });
                }
                ZkSessionState::NotFound => {
                    warn!(
                        session_id = %request.claim.session_id,
                        backend_session_id = %backend_session_id,
                        ?session_type,
                        "backend session not found; marking for resubmission"
                    );
                    self.record_backend_resubmission(
                        request,
                        handle,
                        session_type,
                        &backend_session_id,
                    )
                    .await;
                    return Err(ZkProverError::BackendSessionNotFound { backend_session_id });
                }
            }
        }
    }

    async fn record_backend_resubmission(
        &self,
        request: &ProofGeneratorRequest,
        handle: &ProofSessionHandle<Client>,
        session_type: SessionType,
        backend_session_id: &str,
    ) {
        // Worker upsert rejects terminal states; poll branches log the failure
        // detail, then Submitting makes the next claim replace this session.
        if let Err(error) = handle
            .record(session_type, backend_session_id.to_owned(), BackendSessionState::Submitting)
            .await
        {
            warn!(
                session_id = %request.claim.session_id,
                backend_session_id = %backend_session_id,
                ?session_type,
                error = %error,
                "failed to mark backend session for resubmission"
            );
        }
    }

    async fn with_heartbeat_while_generating<Output, Generate>(
        &self,
        request: &ProofGeneratorRequest,
        generate: Generate,
    ) -> Result<Output, ProofGeneratorError>
    where
        Generate: Future<Output = Result<Output, ZkProverError>>,
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
                match timeout(
                    DEFAULT_PROOF_GENERATOR_HEARTBEAT_FAILURE_DRAIN_TIMEOUT,
                    &mut generate,
                )
                .await
                {
                    Ok(Ok(_)) => {
                        info!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            "discarding zk proof generated after heartbeat failure"
                        );
                    }
                    Ok(Err(error)) => {
                        warn!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            error = %error,
                            "zk proof generation finished with error after heartbeat failure"
                        );
                    }
                    Err(_) => {
                        warn!(
                            session_id = %request.claim.session_id,
                            lock_id = %request.claim.lock_id,
                            worker_id = %request.claim.worker_id,
                            timeout = ?DEFAULT_PROOF_GENERATOR_HEARTBEAT_FAILURE_DRAIN_TIMEOUT,
                            "timed out waiting for zk proof generation after heartbeat failure"
                        );
                        // Keep the recorded Running backend session. A heartbeat failure means
                        // this worker lost its claim, not that the backend job has stopped.
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
        // Submission continues in the spawned task; shutdown cancels through the controller.
        Self::generate_and_submit(self, job).await.map(drop)
    }

    fn shutdown(&self) {
        self.tasks.cancel_submissions();
    }
}

/// Errors raised while generating and dispatching ZK proof submissions.
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
    /// Claimed proof job is not a ZK proof request.
    #[error("proof job {session_id} is not a ZK proof request")]
    UnsupportedProofRequest {
        /// Proof session identifier.
        session_id: String,
    },
    /// ZK proof generation failed.
    #[error("proof generation failed for job {session_id}: {source}")]
    Generate {
        /// Proof session identifier.
        session_id: String,
        /// Underlying proof generation error.
        #[source]
        source: ZkProverError,
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
