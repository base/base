//! JSON-RPC server implementation for the prover service.

use std::fmt;

use base_prover_service_db::ProofRequestRepo;
use base_prover_service_protocol::{
    DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
    ProveBlockRangeRequest, ProveBlockRangeResponse, ProverRequesterApiServer,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::{ErrorCode, ErrorObjectOwned},
};

use crate::WorkerQueueConfig;

mod delete_proof_request;
mod get_proof;
mod list_proofs;
mod prove_block_range;
mod worker_api;

const ERROR_NOT_FOUND: i32 = -32004;
const ERROR_UNAVAILABLE: i32 = -32014;
const ERROR_RESOURCE_EXHAUSTED: i32 = -32016;
const ERROR_FAILED_PRECONDITION: i32 = -32017;

/// Lock duration tuning for the worker job API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerApiConfig {
    /// Lock duration applied when a worker requests `0` seconds.
    pub default_lock_duration_seconds: u32,
    /// Upper bound a worker-requested lock duration is clamped to.
    pub max_lock_duration_seconds: u32,
}

impl WorkerApiConfig {
    /// Create worker API tuning.
    pub const fn new(default_lock_duration_seconds: u32, max_lock_duration_seconds: u32) -> Self {
        let config = Self { default_lock_duration_seconds, max_lock_duration_seconds };
        config.validate();
        config
    }

    /// Panics if the default lock duration exceeds the max.
    pub const fn validate(&self) {
        assert!(
            self.default_lock_duration_seconds <= self.max_lock_duration_seconds,
            "default lock duration must not exceed max lock duration"
        );
    }
}

impl Default for WorkerApiConfig {
    fn default() -> Self {
        Self::new(300, 3600)
    }
}

/// Tuning for the prover service JSON-RPC server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// Shared `retry_count` cap with [`crate::worker::StatusPoller`].
    pub max_proof_retries: i32,
    /// Worker job lock-duration tuning.
    pub worker: WorkerApiConfig,
    /// Shared worker-claim reclaim budget.
    pub worker_queue: WorkerQueueConfig,
}

/// JSON-RPC server implementing the requester and worker API traits.
#[derive(Clone)]
pub struct ProverServiceServer {
    repo: ProofRequestRepo,
    config: ServerConfig,
}

impl fmt::Debug for ProverServiceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverServiceServer").field("config", &self.config).finish_non_exhaustive()
    }
}

impl ProverServiceServer {
    /// Create a new prover service server.
    pub const fn new(repo: ProofRequestRepo, config: ServerConfig) -> Self {
        config.worker.validate();
        Self { repo, config }
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

    async fn delete_proof_request(&self, request: DeleteProofRequest) -> RpcResult<()> {
        self.delete_proof_request_impl(request).await
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
        let config = WorkerApiConfig::new(300, 3600);

        assert_eq!(
            config,
            WorkerApiConfig { default_lock_duration_seconds: 300, max_lock_duration_seconds: 3600 }
        );
    }

    #[test]
    #[should_panic(expected = "default lock duration must not exceed max lock duration")]
    fn worker_api_config_new_rejects_default_greater_than_max() {
        let _ = WorkerApiConfig::new(3601, 3600);
    }
}
