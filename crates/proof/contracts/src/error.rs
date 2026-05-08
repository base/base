//! Error types for shared contract clients.

use thiserror::Error;

/// Error type for contract interactions.
#[derive(Debug, Error)]
pub enum ContractError {
    /// A contract call or onchain interaction failed.
    #[error("{context}: {source}")]
    Call {
        /// Human-readable label for the failed call (e.g. "`BLOCK_INTERVAL` failed").
        context: String,
        /// The underlying Alloy contract error.
        source: alloy_contract::Error,
    },

    /// A provider request failed before a contract call was constructed.
    #[error("{context}: {source}")]
    Provider {
        /// Human-readable label for the failed provider request.
        context: String,
        /// The underlying Alloy transport error.
        source: alloy_transport::TransportError,
    },

    /// A value returned by the contract failed a validation check.
    #[error("{0}")]
    Validation(String),
}
