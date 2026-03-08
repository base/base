//! ZK proof client error types.

use thiserror::Error;

/// Errors that can occur when using the ZK proof client.
#[derive(Debug, Error)]
pub enum ZkProofError {
    /// The gRPC call returned a non-OK status.
    #[error("gRPC status: {0}")]
    GrpcStatus(#[from] tonic::Status),

    /// The endpoint URL is invalid.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

impl ZkProofError {
    /// Returns `true` if this error is transient and the operation can be retried.
    ///
    /// Certain gRPC status codes (`UNAVAILABLE`, `DEADLINE_EXCEEDED`,
    /// `RESOURCE_EXHAUSTED`, `ABORTED`) are considered retryable.
    /// Configuration errors (`InvalidUrl`) and permanent gRPC failures are not.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::GrpcStatus(status) => matches!(
                status.code(),
                tonic::Code::Unavailable
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::ResourceExhausted
                    | tonic::Code::Aborted
            ),
            Self::InvalidUrl(_) => false,
        }
    }
}
