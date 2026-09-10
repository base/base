use std::result;

use alloy_primitives::Address;

/// Errors that can occur in baseline operations.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// Network connection error.
    #[error("network error: {0}")]
    Network(String),

    /// RPC call error.
    #[error("rpc error: {0}")]
    Rpc(String),

    /// Transaction execution error.
    #[error("transaction failed: {0}")]
    Transaction(String),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// Account operation error.
    #[error("account error for {address}: {message}")]
    Account {
        /// Account address.
        address: Address,
        /// Error message.
        message: String,
    },

    /// Workload execution error.
    #[error("workload error: {0}")]
    Workload(String),

    /// Operation timeout.
    #[error("timeout waiting for {operation} after {duration:?}")]
    Timeout {
        /// Operation that timed out.
        operation: String,
        /// Duration waited.
        duration: std::time::Duration,
    },

    /// Transparent error from eyre.
    #[error(transparent)]
    Eyre(#[from] eyre::Report),
}

/// Result type for baseline operations.
pub type Result<T> = result::Result<T, BaselineError>;
