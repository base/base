//! Error types for the proposer.

use base_proof_contracts::ContractError;
use base_proof_rpc::RpcError;
use thiserror::Error;

use crate::Metrics;

/// Main error type for the proposer.
#[derive(Debug, Error)]
pub enum ProposerError {
    /// RPC connection error.
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),

    /// Prover server communication error.
    #[error("Prover error: {0}")]
    Prover(String),

    /// Contract interaction error.
    #[error("Contract error: {0}")]
    Contract(String),

    /// Transaction was included but reverted on-chain.
    #[error("transaction reverted: {0}")]
    TxReverted(String),

    /// The dispute game already exists for the given parameters.
    #[error("game already exists")]
    GameAlreadyExists,

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Internal error.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Output root mismatch between enclave and local computation.
    #[error("output root mismatch: expected {expected}, got {actual}")]
    OutputRootMismatch {
        /// Expected output root.
        expected: alloy_primitives::B256,
        /// Actual output root from enclave.
        actual: alloy_primitives::B256,
    },

    /// L1 origin hash mismatch.
    #[error("L1 origin mismatch: expected {expected}, got {actual}")]
    L1OriginMismatch {
        /// Expected L1 origin hash.
        expected: alloy_primitives::B256,
        /// Actual L1 origin hash.
        actual: alloy_primitives::B256,
    },

    /// Block number mismatch.
    #[error("block number mismatch: expected {expected}, got {actual}")]
    BlockNumberMismatch {
        /// Expected block number.
        expected: u64,
        /// Actual block number.
        actual: u64,
    },

    /// Failed to derive block info.
    #[error("failed to derive block info: {0}")]
    BlockInfoDerivation(String),

    /// Transaction manager error (nonce, fees, RPC, signing, etc.).
    #[error(transparent)]
    TxManager(#[from] base_tx_manager::TxManagerError),
}

impl From<ContractError> for ProposerError {
    fn from(err: ContractError) -> Self {
        Self::Contract(err.to_string())
    }
}

impl From<eyre::Error> for ProposerError {
    fn from(err: eyre::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

impl ProposerError {
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
            Self::OutputRootMismatch { .. } => Metrics::ERROR_TYPE_OUTPUT_ROOT_MISMATCH,
            Self::L1OriginMismatch { .. } => Metrics::ERROR_TYPE_L1_ORIGIN_MISMATCH,
            Self::BlockNumberMismatch { .. } => Metrics::ERROR_TYPE_BLOCK_NUMBER_MISMATCH,
            Self::BlockInfoDerivation(_) => Metrics::ERROR_TYPE_BLOCK_INFO_DERIVATION,
            Self::TxManager(_) => Metrics::ERROR_TYPE_TX_MANAGER,
        }
    }
}

/// Result type alias for proposer operations.
pub type ProposerResult<T> = Result<T, ProposerError>;
