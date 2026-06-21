//! Implementation of the prover worker JSON-RPC endpoints.

use base_prover_service_db::{
    ClaimProofJob, CompleteClaimedProofJob, HeartbeatOutcome, HeartbeatProofJob,
    RecordSessionOutcome, SubmitProofOutcome, WorkerSessionUpsert, canonical_session_id,
};
use base_prover_service_protocol::{
    GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
    HeartbeatRequest, HeartbeatResponse, ProofJob as ProtocolProofJob, ProverWorkerApiServer,
    RecordProofSessionRequest, RecordProofSessionResponse, WorkerSubmitProofRequest,
    WorkerSubmitProofResponse,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::ErrorObjectOwned,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::server::{
    ProverServiceServer, WorkerApiConfig, failed_precondition, internal, invalid_argument,
    not_found, record_rpc_result,
};

#[async_trait]
impl ProverWorkerApiServer for ProverServiceServer {
    async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> RpcResult<GetNextProofResponse> {
        self.get_next_proof_impl(request).await
    }

    async fn heartbeat(&self, request: HeartbeatRequest) -> RpcResult<HeartbeatResponse> {
        self.heartbeat_impl(request).await
    }

    async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> RpcResult<WorkerSubmitProofResponse> {
        self.submit_proof_impl(request).await
    }

    async fn get_proof_session(
        &self,
        request: GetProofSessionRequest,
    ) -> RpcResult<GetProofSessionResponse> {
        self.get_proof_session_impl(request).await
    }

    async fn record_proof_session(
        &self,
        request: RecordProofSessionRequest,
    ) -> RpcResult<RecordProofSessionResponse> {
        self.record_proof_session_impl(request).await
    }
}

impl ProverServiceServer {
    /// Atomically claims and returns the next eligible worker proof job.
    pub async fn get_next_proof_impl(
        &self,
        request: GetNextProofRequest,
    ) -> RpcResult<GetNextProofResponse> {
        let start = std::time::Instant::now();
        let result = self.get_next_proof_inner(request).await;
        record_rpc_result("GetNextProof", start, &result);

        result
    }

    async fn get_next_proof_inner(
        &self,
        request: GetNextProofRequest,
    ) -> RpcResult<GetNextProofResponse> {
        let claim = ClaimProofJob {
            worker_id: request.worker_id,
            api_proof_type: request.proof_type.into(),
            tee_kinds: request.tee_kinds.into_iter().map(Into::into).collect(),
            zk_vms: request.zk_vms.into_iter().map(Into::into).collect(),
            lock_duration_seconds: resolve_lock_duration(
                self.config.worker,
                request.lock_duration_seconds,
            ),
            max_attempts: self.config.worker_queue.reclaim_attempts,
        };

        let job = self
            .repo
            .claim_next_proof_job(claim)
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        match job {
            Some(job) => {
                let worker_id = job.worker_id.as_deref().unwrap_or("<unknown>");
                info!(
                    worker_id = %worker_id,
                    session_id = %job.session_id,
                    api_proof_type = %job.api_proof_type,
                    attempt = job.attempt,
                    "claimed proof job for worker"
                );
                Ok(GetNextProofResponse { job: Some(into_protocol_job(job)?) })
            }
            None => Ok(GetNextProofResponse { job: None }),
        }
    }

    /// Extends the lock on a worker-owned proof job.
    pub async fn heartbeat_impl(&self, request: HeartbeatRequest) -> RpcResult<HeartbeatResponse> {
        let start = std::time::Instant::now();
        let result = self.heartbeat_inner(request).await;
        record_rpc_result("Heartbeat", start, &result);

        result
    }

    async fn heartbeat_inner(&self, request: HeartbeatRequest) -> RpcResult<HeartbeatResponse> {
        let HeartbeatRequest { session_id, lock_id, worker_id, lock_duration_seconds } = request;
        let canonical_session_id =
            canonical_session_id(&session_id).map_err(|e| invalid_argument(format!("{e}")))?;
        let lock_id = parse_lock_id(&lock_id)?;

        let outcome = self
            .repo
            .heartbeat_proof_job(HeartbeatProofJob {
                session_id: canonical_session_id,
                lock_id,
                worker_id,
                lock_duration_seconds: resolve_lock_duration(
                    self.config.worker,
                    lock_duration_seconds,
                ),
            })
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        match outcome {
            HeartbeatOutcome::Updated(job) => {
                Ok(HeartbeatResponse { job: into_protocol_job(job)? })
            }
            HeartbeatOutcome::NotFound => {
                Err(not_found(format!("proof job not found for session_id {session_id}")))
            }
            HeartbeatOutcome::NotClaimed(_) => {
                Err(reject_ownership("heartbeat", &session_id, "job is not currently claimed"))
            }
            HeartbeatOutcome::StaleLock(_) => Err(reject_ownership(
                "heartbeat",
                &session_id,
                "lock is held by another worker or has been rotated",
            )),
            HeartbeatOutcome::Expired(_) => {
                Err(reject_ownership("heartbeat", &session_id, "lock has expired"))
            }
            HeartbeatOutcome::Terminal(_) => Err(reject_ownership(
                "heartbeat",
                &session_id,
                "job has already reached a terminal state",
            )),
            HeartbeatOutcome::Unknown(_) => {
                Err(reject_ownership("heartbeat", &session_id, "lock is no longer valid"))
            }
        }
    }

    /// Records a worker proof submission and completes the job.
    pub async fn submit_proof_impl(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> RpcResult<WorkerSubmitProofResponse> {
        let start = std::time::Instant::now();
        let result = self.submit_proof_inner(request).await;
        record_rpc_result("SubmitProof", start, &result);

        result
    }

    async fn submit_proof_inner(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> RpcResult<WorkerSubmitProofResponse> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        let lock_id = parse_lock_id(&request.lock_id)?;

        let outcome = self
            .repo
            .complete_claimed_proof_job(CompleteClaimedProofJob {
                session_id,
                lock_id,
                worker_id: request.worker_id.clone(),
                result: request.result,
            })
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        match outcome {
            SubmitProofOutcome::Completed(job) => {
                info!(
                    worker_id = %request.worker_id,
                    session_id = %request.session_id,
                    "worker submitted proof result"
                );
                Ok(WorkerSubmitProofResponse { job: into_protocol_job(job)? })
            }
            SubmitProofOutcome::ResultMismatch { reason, .. } => {
                warn!(
                    worker_id = %request.worker_id,
                    session_id = %request.session_id,
                    reason = %reason,
                    "rejected worker proof submission: result does not match claimed job"
                );
                Err(invalid_argument(format!(
                    "submit_proof rejected for session_id {}: {reason}",
                    request.session_id
                )))
            }
            SubmitProofOutcome::ResultConflict { .. } => {
                warn!(
                    worker_id = %request.worker_id,
                    session_id = %request.session_id,
                    "rejected worker proof submission: job already completed with a different result"
                );
                Err(failed_precondition(format!(
                    "submit_proof rejected for session_id {}: job already completed with a different result",
                    request.session_id
                )))
            }
            SubmitProofOutcome::NotFound => {
                Err(not_found(format!("proof job not found for session_id {}", request.session_id)))
            }
            SubmitProofOutcome::NotClaimed(_) => Err(reject_ownership(
                "submit_proof",
                &request.session_id,
                "job is not currently claimed",
            )),
            SubmitProofOutcome::StaleLock(_) => Err(reject_ownership(
                "submit_proof",
                &request.session_id,
                "lock is held by another worker or has been rotated",
            )),
            SubmitProofOutcome::Expired(_) => {
                Err(reject_ownership("submit_proof", &request.session_id, "lock has expired"))
            }
            SubmitProofOutcome::Terminal(_) => Err(reject_ownership(
                "submit_proof",
                &request.session_id,
                "job has already reached a terminal state",
            )),
            SubmitProofOutcome::Unknown(_) => Err(reject_ownership(
                "submit_proof",
                &request.session_id,
                "lock is no longer valid",
            )),
        }
    }

    /// Returns the active backend session recorded for a claimed proof job.
    pub async fn get_proof_session_impl(
        &self,
        request: GetProofSessionRequest,
    ) -> RpcResult<GetProofSessionResponse> {
        let start = std::time::Instant::now();
        let result = self.get_proof_session_inner(request).await;
        record_rpc_result("GetProofSession", start, &result);

        result
    }

    async fn get_proof_session_inner(
        &self,
        request: GetProofSessionRequest,
    ) -> RpcResult<GetProofSessionResponse> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;

        let session = self
            .repo
            .get_active_session(&session_id, request.session_type.into())
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        Ok(GetProofSessionResponse { session: session.map(Into::into) })
    }

    /// Records (inserts or updates) the backend session for a claimed proof job.
    pub async fn record_proof_session_impl(
        &self,
        request: RecordProofSessionRequest,
    ) -> RpcResult<RecordProofSessionResponse> {
        let start = std::time::Instant::now();
        let result = self.record_proof_session_inner(request).await;
        record_rpc_result("RecordProofSession", start, &result);

        result
    }

    async fn record_proof_session_inner(
        &self,
        request: RecordProofSessionRequest,
    ) -> RpcResult<RecordProofSessionResponse> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        let lock_id = parse_lock_id(&request.lock_id)?;

        let outcome = self
            .repo
            .record_worker_proof_session(WorkerSessionUpsert {
                session_id,
                lock_id,
                worker_id: request.worker_id.clone(),
                session_type: request.session_type.into(),
                backend_session_id: request.backend_session_id,
                status: request.state.into(),
                error_message: None,
            })
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        match outcome {
            RecordSessionOutcome::Recorded(session) => {
                info!(
                    worker_id = %request.worker_id,
                    session_id = %request.session_id,
                    backend_session_id = %session.backend_session_id,
                    "recorded backend session for worker"
                );
                Ok(RecordProofSessionResponse { session: session.into() })
            }
            RecordSessionOutcome::TerminalBackendSession(session) => {
                info!(
                    worker_id = %request.worker_id,
                    session_id = %request.session_id,
                    backend_session_id = %session.backend_session_id,
                    "backend session was already terminal"
                );
                Ok(RecordProofSessionResponse { session: session.into() })
            }
            RecordSessionOutcome::NotFound => {
                Err(not_found(format!("proof job not found for session_id {}", request.session_id)))
            }
            RecordSessionOutcome::NotClaimed => Err(reject_ownership(
                "record_proof_session",
                &request.session_id,
                "job is not currently claimed",
            )),
            RecordSessionOutcome::StaleLock => Err(reject_ownership(
                "record_proof_session",
                &request.session_id,
                "lock is held by another worker or has been rotated",
            )),
            RecordSessionOutcome::Expired => Err(reject_ownership(
                "record_proof_session",
                &request.session_id,
                "lock has expired",
            )),
            RecordSessionOutcome::Terminal => Err(reject_ownership(
                "record_proof_session",
                &request.session_id,
                "job has already reached a terminal state",
            )),
            RecordSessionOutcome::TerminalSessionStatus => Err(reject_ownership(
                "record_proof_session",
                &request.session_id,
                "backend session status is terminal",
            )),
        }
    }
}

/// `0` uses the configured default; any value is clamped to the maximum.
const fn resolve_lock_duration(config: WorkerApiConfig, requested: u32) -> u32 {
    let duration = if requested == 0 { config.default_lock_duration_seconds } else { requested };

    if duration > config.max_lock_duration_seconds {
        config.max_lock_duration_seconds
    } else {
        duration
    }
}

/// Parse a worker fencing token. A malformed token can never own a job, so it is
/// rejected as a non-retryable precondition failure.
fn parse_lock_id(lock_id: &str) -> RpcResult<Uuid> {
    Uuid::parse_str(lock_id).map_err(|e| {
        failed_precondition(format!("lock_id {lock_id} is not a valid fencing token: {e}"))
    })
}

fn into_protocol_job(job: base_prover_service_db::ProofJob) -> RpcResult<ProtocolProofJob> {
    ProtocolProofJob::try_from(job).map_err(|e| internal(e.to_string()))
}

/// Build a non-retryable ownership rejection and log it for diagnostics.
fn reject_ownership(method: &str, session_id: &str, reason: &str) -> ErrorObjectOwned {
    warn!(method = %method, session_id = %session_id, reason = %reason, "rejected worker ownership request");
    failed_precondition(format!("{method} rejected for session_id {session_id}: {reason}"))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use uuid::Uuid;

    use super::{WorkerApiConfig, parse_lock_id, resolve_lock_duration};

    #[rstest]
    #[case::zero_uses_default(0, 300)]
    #[case::above_max_clamps(3601, 3600)]
    #[case::in_range_passes_through(120, 120)]
    fn resolve_lock_duration_resolves(#[case] requested: u32, #[case] expected: u32) {
        assert_eq!(resolve_lock_duration(WorkerApiConfig::default(), requested), expected);
    }

    #[test]
    fn parse_lock_id_accepts_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(parse_lock_id(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn parse_lock_id_rejects_malformed_token() {
        let err = parse_lock_id("not-a-uuid").unwrap_err();
        assert_eq!(err.code(), super::super::ERROR_FAILED_PRECONDITION);
    }
}
