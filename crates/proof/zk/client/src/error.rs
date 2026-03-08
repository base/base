//! ZK proof client error types.

use thiserror::Error;

/// Errors that can occur when using the ZK proof client.
#[derive(Debug, Error)]
pub enum ZkProofError {
    /// Failed to establish a gRPC connection.
    #[error("connection error: {0}")]
    Connection(#[from] tonic::transport::Error),

    /// The gRPC call returned a non-OK status.
    #[error("gRPC status: {0}")]
    GrpcStatus(#[from] tonic::Status),

    /// The request timed out.
    #[error("request timed out: {0}")]
    Timeout(String),

    /// The endpoint URL is invalid.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

impl ZkProofError {
    /// Returns `true` if this error is transient and the operation can be retried.
    ///
    /// Connection failures, timeouts, and certain gRPC status codes
    /// (`UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`) are considered
    /// retryable. Configuration errors (`InvalidUrl`) and permanent gRPC failures
    /// are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Connection(_) | Self::Timeout(_) => true,
            Self::GrpcStatus(status) => matches!(
                status.code(),
                tonic::Code::Unavailable
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::ResourceExhausted
            ),
            Self::InvalidUrl(_) => false,
        }
    }
}
