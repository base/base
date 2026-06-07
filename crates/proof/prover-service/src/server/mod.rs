//! JSON-RPC server implementation for the prover service.

use std::fmt;

use base_prover_service_db::ProofRequestRepo;
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::{ErrorCode, ErrorObjectOwned},
};

use crate::ProofRequestManager;

mod get_proof;
mod list_proofs;
mod prove_block_range;
mod worker_api;

const ERROR_NOT_FOUND: i32 = -32004;
const ERROR_UNAVAILABLE: i32 = -32014;
const ERROR_RESOURCE_EXHAUSTED: i32 = -32016;
const ERROR_FAILED_PRECONDITION: i32 = -32017;

/// Tunable defaults for the prover worker job API (`getNextProof` / `heartbeat`).
///
/// Workers may request a specific lock duration; a request of `0` falls back to
/// [`Self::default_lock_duration_seconds`] and any request is clamped to
/// [`Self::max_lock_duration_seconds`]. [`Self::max_claim_attempts`] bounds how
/// many times an expired claim may be reclaimed before the reaper fails it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerApiConfig {
    /// Lock duration applied when a worker requests `0` seconds.
    pub default_lock_duration_seconds: u32,
    /// Upper bound a worker-requested lock duration is clamped to.
    pub max_lock_duration_seconds: u32,
    /// Reclaim budget: expired claims are reclaimable while `attempt < max_claim_attempts`.
    pub max_claim_attempts: u32,
}

impl WorkerApiConfig {
    /// Default worker API tuning used by [`ProverServiceServer::new`].
    pub const DEFAULT: Self = Self::new(300, 3600, 5);

    /// Create worker API tuning with `default_lock_duration_seconds <= max_lock_duration_seconds`.
    pub const fn new(
        default_lock_duration_seconds: u32,
        max_lock_duration_seconds: u32,
        max_claim_attempts: u32,
    ) -> Self {
        let config =
            Self { default_lock_duration_seconds, max_lock_duration_seconds, max_claim_attempts };
        config.validate();
        config
    }

    /// Enforce the single config invariant. Panics if it is violated.
    pub const fn validate(&self) {
        assert!(
            self.default_lock_duration_seconds <= self.max_lock_duration_seconds,
            "default lock duration must not exceed max lock duration"
        );
    }
}

impl Default for WorkerApiConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// JSON-RPC server implementing the requester and worker API traits.
#[derive(Clone)]
pub struct ProverServiceServer {
    repo: ProofRequestRepo,
    manager: ProofRequestManager,
    /// Shared `retry_count` cap with [`crate::worker::StatusPoller`] (same as `retry_or_fail_stuck_request`).
    max_proof_retries: i32,
    /// Worker job API tuning (lock durations and reclaim budget).
    worker: WorkerApiConfig,
}

impl fmt::Debug for ProverServiceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverServiceServer")
            .field("max_proof_retries", &self.max_proof_retries)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl ProverServiceServer {
    /// Create a new prover service server with default worker API tuning.
    pub const fn new(
        repo: ProofRequestRepo,
        manager: ProofRequestManager,
        max_proof_retries: i32,
    ) -> Self {
        Self { repo, manager, max_proof_retries, worker: WorkerApiConfig::DEFAULT }
    }

    /// Override the worker job API tuning.
    #[must_use]
    pub const fn with_worker_config(mut self, worker: WorkerApiConfig) -> Self {
        worker.validate();
        self.worker = worker;
        self
    }
}

#[async_trait]
impl ProverRequesterApiServer for ProverServiceServer {
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> RpcResult<ProveBlockRangeResponse> {
        self.prove_block_range_impl(request).await
    }

    async fn get_proof(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
        self.get_proof_impl(request).await
    }

    async fn list_proofs(&self, request: ListProofsRequest) -> RpcResult<ListProofsResponse> {
        self.list_proofs_impl(request).await
    }
}

fn invalid_argument(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), message.into(), None::<()>)
}

fn not_found(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERROR_NOT_FOUND, message.into(), None::<()>)
}

fn internal(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ErrorCode::InternalError.code(), message.into(), None::<()>)
}

fn unavailable(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERROR_UNAVAILABLE, message.into(), None::<()>)
}

fn resource_exhausted(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERROR_RESOURCE_EXHAUSTED, message.into(), None::<()>)
}

fn failed_precondition(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERROR_FAILED_PRECONDITION, message.into(), None::<()>)
}

const fn rpc_status_code_str(code: i32) -> &'static str {
    match code {
        code if code == ErrorCode::InvalidParams.code() => "INVALID_ARGUMENT",
        code if code == ErrorCode::InternalError.code() => "INTERNAL",
        ERROR_NOT_FOUND => "NOT_FOUND",
        ERROR_UNAVAILABLE => "UNAVAILABLE",
        ERROR_RESOURCE_EXHAUSTED => "RESOURCE_EXHAUSTED",
        ERROR_FAILED_PRECONDITION => "FAILED_PRECONDITION",
        _ => "ERROR",
    }
}

fn record_rpc_result<T>(method: &str, start: std::time::Instant, result: &RpcResult<T>) {
    let (success, status_code) = match result {
        Ok(_) => (true, "OK"),
        Err(error) => (false, rpc_status_code_str(error.code())),
    };
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    crate::metrics::inc_requests(method, success, status_code);
    crate::metrics::record_response_latency(method, success, elapsed_ms);
}

#[cfg(test)]
mod tests {
    use super::WorkerApiConfig;

    #[test]
    fn worker_api_config_new_accepts_valid_durations() {
        let config = WorkerApiConfig::new(300, 3600, 5);

        assert_eq!(
            config,
            WorkerApiConfig {
                default_lock_duration_seconds: 300,
                max_lock_duration_seconds: 3600,
                max_claim_attempts: 5,
            }
        );
    }

    #[test]
    #[should_panic(expected = "default lock duration must not exceed max lock duration")]
    fn worker_api_config_new_rejects_default_greater_than_max() {
        let _ = WorkerApiConfig::new(3601, 3600, 5);
    }
}
