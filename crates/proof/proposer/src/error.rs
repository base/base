//! Error types for the proposer.

use base_proof_rpc::RpcError;
use thiserror::Error;

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
    /// Metric label for RPC errors.
    pub const ERROR_TYPE_RPC: &str = "rpc";
    /// Metric label for prover errors.
    pub const ERROR_TYPE_PROVER: &str = "prover";
    /// Metric label for contract interaction errors.
    pub const ERROR_TYPE_CONTRACT: &str = "contract";
    /// Metric label for reverted transactions.
    pub const ERROR_TYPE_TX_REVERTED: &str = "tx_reverted";
    /// Metric label for configuration errors.
    pub const ERROR_TYPE_CONFIG: &str = "config";
    /// Metric label for internal errors.
    pub const ERROR_TYPE_INTERNAL: &str = "internal";
    /// Metric label for transaction manager errors.
    pub const ERROR_TYPE_TX_MANAGER: &str = "tx_manager";
    /// Metric label for duplicate game errors.
    pub const ERROR_TYPE_GAME_ALREADY_EXISTS: &str = "game_already_exists";

    /// Returns true if this error indicates the game already exists.
    pub const fn is_game_already_exists(&self) -> bool {
        matches!(self, Self::GameAlreadyExists)
    }

    /// Returns the metrics label for this error variant.
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::Rpc(_) => Self::ERROR_TYPE_RPC,
            Self::Prover(_) => Self::ERROR_TYPE_PROVER,
            Self::Contract(_) => Self::ERROR_TYPE_CONTRACT,
            Self::TxReverted(_) => Self::ERROR_TYPE_TX_REVERTED,
            Self::GameAlreadyExists => Self::ERROR_TYPE_GAME_ALREADY_EXISTS,
            Self::Config(_) => Self::ERROR_TYPE_CONFIG,
            Self::Internal(_) => Self::ERROR_TYPE_INTERNAL,
            Self::TxManager(_) => Self::ERROR_TYPE_TX_MANAGER,
        }
    }
}

/// Result type alias for proposer operations.
pub type ProposerResult<T> = Result<T, ProposerError>;
