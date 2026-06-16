//! Error types for proof submission.

use alloy_primitives::B256;
use base_proof_primitives::CryptoError;
use base_tx_manager::TxManagerError;
use thiserror::Error;

/// Errors produced while encoding or submitting aggregate proof bytes.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProofSubmissionError {
    /// The target dispute game already exists for the submitted parameters.
    #[error("game already exists")]
    GameAlreadyExists,

    /// A proof of this type has already been attached to the dispute game.
    #[error("proof already verified")]
    ProofAlreadyVerified,

    /// The proof's L1 origin is older than the EIP-2935 history window.
    #[error("l1 origin too old")]
    L1OriginTooOld,

    /// The parent game is no longer valid on-chain.
    #[error("invalid parent game")]
    InvalidParentGame,

    /// The proof signer is not valid on-chain.
    #[error("invalid signer")]
    InvalidSigner,

    /// The transaction was included but reverted on-chain.
    #[error("transaction {0} reverted")]
    TxReverted(B256),

    /// Transaction manager error while submitting the proof transaction.
    #[error(transparent)]
    TxManager(#[from] TxManagerError),

    /// Proof byte encoding failed before submission.
    #[error("proof encoding failed: {0}")]
    Encode(#[from] CryptoError),
}

impl ProofSubmissionError {
    /// Returns true if this error indicates the game already exists.
    pub const fn is_game_already_exists(&self) -> bool {
        matches!(self, Self::GameAlreadyExists)
    }

    /// Returns true if this error indicates the proof was already attached.
    pub const fn is_proof_already_verified(&self) -> bool {
        matches!(self, Self::ProofAlreadyVerified)
    }

    /// Returns true if this error indicates the proof's L1 origin is too old.
    pub const fn is_l1_origin_too_old(&self) -> bool {
        matches!(self, Self::L1OriginTooOld)
    }

    /// Returns true if this error indicates the parent game is invalid.
    pub const fn is_invalid_parent_game(&self) -> bool {
        matches!(self, Self::InvalidParentGame)
    }

    /// Returns true if this error indicates the proof signer is invalid.
    pub const fn is_invalid_signer(&self) -> bool {
        matches!(self, Self::InvalidSigner)
    }
}
