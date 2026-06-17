//! Error types for proof submission.

use alloy_primitives::B256;
use base_tx_manager::TxManagerError;
use thiserror::Error;

/// Errors produced while submitting aggregate proof bytes.
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
    TxManager(TxManagerError),
}
