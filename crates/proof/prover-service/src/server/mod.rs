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

const ERROR_NOT_FOUND: i32 = -32004;
const ERROR_UNAVAILABLE: i32 = -32014;
const ERROR_RESOURCE_EXHAUSTED: i32 = -32016;
const ERROR_FAILED_PRECONDITION: i32 = -32017;

/// JSON-RPC server implementing the requester API trait.
#[derive(Clone)]
pub struct ProverServiceServer {
    repo: ProofRequestRepo,
    manager: ProofRequestManager,
    /// Shared `retry_count` cap with [`crate::worker::StatusPoller`] (same as `retry_or_fail_stuck_request`).
    max_proof_retries: i32,
}

impl fmt::Debug for ProverServiceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverServiceServer")
            .field("max_proof_retries", &self.max_proof_retries)
            .finish_non_exhaustive()
    }
}

impl ProverServiceServer {
    /// Create a new prover service server.
    pub const fn new(
        repo: ProofRequestRepo,
        manager: ProofRequestManager,
        max_proof_retries: i32,
    ) -> Self {
        Self { repo, manager, max_proof_retries }
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
