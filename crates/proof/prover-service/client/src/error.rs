//! Shared prover-service client error types.

use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorCode};
use thiserror::Error;

/// Errors that can occur when constructing or using prover-service clients.
#[derive(Debug, Error)]
pub enum ProverServiceClientError {
    /// The client configuration is invalid.
    #[error("invalid prover-service client config: {0}")]
    InvalidConfig(String),

    /// A JSON-RPC client, server, or transport error occurred.
    #[error("prover-service RPC/transport failure: {0}")]
    RpcTransport(#[from] JsonRpcClientError),

    /// The prover service reached a terminal failed proof state.
    #[error("proof failed: {message}")]
    ProofFailure {
        /// Failure message returned by the prover service.
        message: String,
    },

    /// The prover service rejected a worker lease operation.
    #[error("worker lease rejected: {message}")]
    WorkerLeaseRejected {
        /// Rejection message returned by the prover service.
        message: String,
    },

    /// The client stopped waiting before the proof reached a terminal state.
    #[error("timed out waiting for prover-service result: {0}")]
    Timeout(String),

    /// The service reported success without returning the expected result.
    #[error("missing prover-service result: {0}")]
    MissingResult(String),

    /// The service returned a result payload that does not match the requested proof type.
    #[error("unexpected prover-service result payload: {0}")]
    UnexpectedResultPayload(String),
}

impl ProverServiceClientError {
    /// JSON-RPC code used by the prover service when a dependency is unavailable.
    pub const ERROR_UNAVAILABLE: i32 = -32014;

    /// JSON-RPC code used by the prover service when capacity is exhausted.
    pub const ERROR_RESOURCE_EXHAUSTED: i32 = -32016;

    /// JSON-RPC code used by the prover service for failed preconditions.
    pub const ERROR_FAILED_PRECONDITION: i32 = -32017;

    /// Returns `true` when retrying the same client operation may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RpcTransport(err) => Self::is_retryable_rpc_error(err),
            Self::Timeout(_) => true,
            Self::InvalidConfig(_)
            | Self::ProofFailure { .. }
            | Self::WorkerLeaseRejected { .. }
            | Self::MissingResult(_)
            | Self::UnexpectedResultPayload(_) => false,
        }
    }

    /// Returns `true` when the JSON-RPC error is classified as transient.
    #[must_use]
    pub fn is_retryable_rpc_error(err: &JsonRpcClientError) -> bool {
        match err {
            JsonRpcClientError::Call(call) => Self::is_retryable_rpc_code(call.code()),
            JsonRpcClientError::Transport(_)
            | JsonRpcClientError::RequestTimeout
            | JsonRpcClientError::RestartNeeded(_)
            | JsonRpcClientError::ServiceDisconnect => true,
            JsonRpcClientError::ParseError(_)
            | JsonRpcClientError::InvalidSubscriptionId
            | JsonRpcClientError::InvalidRequestId(_)
            | JsonRpcClientError::Custom(_)
            | JsonRpcClientError::HttpNotImplemented
            | JsonRpcClientError::EmptyBatchRequest(_)
            | JsonRpcClientError::RegisterMethod(_) => false,
        }
    }

    /// Returns `true` when the JSON-RPC error code is classified as transient.
    #[must_use]
    pub const fn is_retryable_rpc_code(code: i32) -> bool {
        code == Self::ERROR_UNAVAILABLE || code == ErrorCode::InternalError.code()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use jsonrpsee::types::ErrorObjectOwned;

    use super::*;

    #[test]
    fn retry_classification_marks_transient_rpc_failures_retryable() {
        let request_timeout = ProverServiceClientError::from(JsonRpcClientError::RequestTimeout);
        let transport = ProverServiceClientError::from(JsonRpcClientError::Transport(
            io::Error::other("connection refused").into(),
        ));
        let unavailable =
            ProverServiceClientError::from(JsonRpcClientError::Call(ErrorObjectOwned::owned(
                ProverServiceClientError::ERROR_UNAVAILABLE,
                "service unavailable",
                None::<()>,
            )));

        assert!(request_timeout.is_retryable());
        assert!(transport.is_retryable());
        assert!(unavailable.is_retryable());
    }

    #[test]
    fn retry_classification_marks_terminal_service_failures_non_retryable() {
        let proof_failure =
            ProverServiceClientError::ProofFailure { message: "proof failed".to_owned() };
        let missing_result =
            ProverServiceClientError::MissingResult("succeeded response had no result".to_owned());
        let unexpected_payload =
            ProverServiceClientError::UnexpectedResultPayload("expected TEE proof".to_owned());
        let resource_exhausted =
            ProverServiceClientError::from(JsonRpcClientError::Call(ErrorObjectOwned::owned(
                ProverServiceClientError::ERROR_RESOURCE_EXHAUSTED,
                "proof retries exhausted; use get_proof",
                None::<()>,
            )));

        assert!(!proof_failure.is_retryable());
        assert!(!missing_result.is_retryable());
        assert!(!unexpected_payload.is_retryable());
        assert!(!resource_exhausted.is_retryable());
    }

    #[test]
    fn retry_classification_marks_worker_lease_rejections_non_retryable() {
        let lease_rejection =
            ProverServiceClientError::WorkerLeaseRejected { message: "lease mismatch".to_owned() };
        let failed_precondition =
            ProverServiceClientError::from(JsonRpcClientError::Call(ErrorObjectOwned::owned(
                ProverServiceClientError::ERROR_FAILED_PRECONDITION,
                "lease is not owned by this worker",
                None::<()>,
            )));

        assert!(!lease_rejection.is_retryable());
        assert!(!failed_precondition.is_retryable());
    }

    #[test]
    fn retry_classification_marks_invalid_config_non_retryable() {
        let invalid_config =
            ProverServiceClientError::InvalidConfig("endpoint URL must include a host".to_owned());

        assert!(!invalid_config.is_retryable());
    }
}
