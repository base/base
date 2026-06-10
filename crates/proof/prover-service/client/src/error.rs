//! Shared prover-service client error types.

use base_prover_service_protocol::PROOF_REQUEST_NOT_FOUND_MESSAGE;
use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorCode};
use thiserror::Error;

/// Errors that can occur when using prover-service clients.
#[derive(Debug, Error)]
pub enum ProverServiceClientError {
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
    /// JSON-RPC code used by the prover service when the requested session does not exist.
    pub const ERROR_NOT_FOUND: i32 = -32004;

    /// JSON-RPC code used by the prover service when a dependency is unavailable.
    pub const ERROR_UNAVAILABLE: i32 = -32014;

    /// JSON-RPC code used by the prover service when the proof request retry budget is exhausted.
    pub const ERROR_RESOURCE_EXHAUSTED: i32 = -32016;

    /// JSON-RPC code used by the prover service for failed preconditions.
    pub const ERROR_FAILED_PRECONDITION: i32 = -32017;

    /// Returns `true` when retrying the same client operation may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RpcTransport(err) => Self::is_retryable_rpc_error(err),
            Self::Timeout(_) => true,
            Self::ProofFailure { .. }
            | Self::WorkerLeaseRejected { .. }
            | Self::MissingResult(_)
            | Self::UnexpectedResultPayload(_) => false,
        }
    }

    /// Returns `true` when the prover service responded that the session does not exist.
    ///
    /// The proposer uses this to distinguish "no session yet, dispatch needed"
    /// from other `NOT_FOUND` cases, such as a succeeded session whose result
    /// payload is unexpectedly unavailable.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        let Self::RpcTransport(JsonRpcClientError::Call(call)) = self else { return false };
        call.code() == Self::ERROR_NOT_FOUND && call.message() == PROOF_REQUEST_NOT_FOUND_MESSAGE
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
            _ => false,
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
    use rstest::rstest;

    use super::*;

    fn rpc_call_error(code: i32, message: &str) -> ProverServiceClientError {
        ProverServiceClientError::from(JsonRpcClientError::Call(ErrorObjectOwned::owned(
            code, message, None::<()>,
        )))
    }

    #[rstest]
    #[case::request_timeout(
        ProverServiceClientError::from(JsonRpcClientError::RequestTimeout),
        true
    )]
    #[case::transport(
        ProverServiceClientError::from(JsonRpcClientError::Transport(
            io::Error::other("connection refused").into(),
        )),
        true
    )]
    #[case::unavailable(
        rpc_call_error(ProverServiceClientError::ERROR_UNAVAILABLE, "service unavailable"),
        true
    )]
    #[case::timeout(ProverServiceClientError::Timeout("proof not ready".to_owned()), true)]
    #[case::proof_failure(
        ProverServiceClientError::ProofFailure { message: "proof failed".to_owned() },
        false
    )]
    #[case::worker_lease_rejected(
        ProverServiceClientError::WorkerLeaseRejected { message: "lease mismatch".to_owned() },
        false
    )]
    #[case::missing_result(
        ProverServiceClientError::MissingResult("succeeded response had no result".to_owned()),
        false
    )]
    #[case::unexpected_payload(
        ProverServiceClientError::UnexpectedResultPayload("expected TEE proof".to_owned()),
        false
    )]
    #[case::resource_exhausted(
        rpc_call_error(
            ProverServiceClientError::ERROR_RESOURCE_EXHAUSTED,
            "proof retries exhausted; use get_proof"
        ),
        false
    )]
    #[case::failed_precondition(
        rpc_call_error(
            ProverServiceClientError::ERROR_FAILED_PRECONDITION,
            "lease is not owned by this worker"
        ),
        false
    )]
    fn retry_classification_matches_expected(
        #[case] error: ProverServiceClientError,
        #[case] expected_retryable: bool,
    ) {
        assert_eq!(error.is_retryable(), expected_retryable);
    }

    #[test]
    fn is_not_found_matches_only_missing_session_not_found() {
        let not_found = rpc_call_error(
            ProverServiceClientError::ERROR_NOT_FOUND,
            PROOF_REQUEST_NOT_FOUND_MESSAGE,
        );
        assert!(not_found.is_not_found());

        let missing_result =
            rpc_call_error(ProverServiceClientError::ERROR_NOT_FOUND, "proof result not available");
        assert!(!missing_result.is_not_found());

        let other_call = rpc_call_error(ProverServiceClientError::ERROR_UNAVAILABLE, "down");
        assert!(!other_call.is_not_found());

        let transport: ProverServiceClientError =
            JsonRpcClientError::Transport(io::Error::other("connection refused").into()).into();
        assert!(!transport.is_not_found());

        let timeout = ProverServiceClientError::Timeout("not ready".into());
        assert!(!timeout.is_not_found());
    }
}
