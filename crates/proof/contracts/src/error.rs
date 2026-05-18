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

impl ContractError {
    /// Creates an error for a failed contract call.
    pub fn call(context: impl Into<String>, source: alloy_contract::Error) -> Self {
        Self::Call { context: context.into(), source }
    }

    /// Creates an error for a failed provider request.
    pub fn provider(context: impl Into<String>, source: alloy_transport::TransportError) -> Self {
        Self::Provider { context: context.into(), source }
    }

    /// Creates an error for a failed contract value validation.
    pub fn validation(context: impl Into<String>) -> Self {
        Self::Validation(context.into())
    }
}
