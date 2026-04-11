//! Error types for the proposer.

use base_proof_rpc::RpcError;
use thiserror::Error;

use crate::Metrics;

/// Main error type for the proposer.
#[derive(Debug, Error)]
pub enum ProposerError {
    /// RPC communication error.
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),

    /// Prover server error.
    #[error("prover error: {0}")]
    Prover(String),

    /// Contract interaction error.
    #[error("contract error: {0}")]
    Contract(String),

    /// Transaction was included but reverted on-chain.
    #[error("transaction reverted: {0}")]
    TxReverted(String),

    /// The dispute game already exists for the given parameters.
    #[error("game already exists")]
    GameAlreadyExists,

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// Internal logic error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Transaction manager error (nonce, fees, signing, etc.).
    #[error(transparent)]
    TxManager(#[from] base_tx_manager::TxManagerError),
}

impl ProposerError {
    /// Returns true if this error indicates the game already exists.
    pub const fn is_game_already_exists(&self) -> bool {
        matches!(self, Self::GameAlreadyExists)
    }

    /// Returns the metrics label for this error variant.
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::Rpc(_) => Metrics::ERROR_TYPE_RPC,
            Self::Prover(_) => Metrics::ERROR_TYPE_PROVER,
            Self::Contract(_) => Metrics::ERROR_TYPE_CONTRACT,
            Self::TxReverted(_) => Metrics::ERROR_TYPE_TX_REVERTED,
            Self::GameAlreadyExists => Metrics::ERROR_TYPE_GAME_ALREADY_EXISTS,
            Self::Config(_) => Metrics::ERROR_TYPE_CONFIG,
            Self::Internal(_) => Metrics::ERROR_TYPE_INTERNAL,
            Self::TxManager(_) => Metrics::ERROR_TYPE_TX_MANAGER,
        }
    }
}

/// Result type alias for proposer operations.
pub type ProposerResult<T> = Result<T, ProposerError>;
