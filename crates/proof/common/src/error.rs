//! Error types for shared contract clients.

use thiserror::Error;

/// Error type for contract interactions.
#[derive(Debug, Error)]
pub enum ContractError {
    /// A contract call or on-chain interaction failed.
    #[error("contract call failed: {0}")]
    Call(String),
}
