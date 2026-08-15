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

    /// The parent game is no longer valid onchain.
    #[error("invalid parent game")]
    InvalidParentGame,

    /// The proof signer is not valid onchain.
    #[error("invalid signer")]
    InvalidSigner,

    /// The transaction was included but reverted onchain.
    #[error("transaction {0} reverted")]
    TxReverted(B256),

    /// Transaction manager error while submitting the proof transaction.
    #[error(transparent)]
    TxManager(TxManagerError),
}

impl ProofSubmissionError {
    /// Returns the metrics label for this error variant.
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::GameAlreadyExists => "game_already_exists",
            Self::ProofAlreadyVerified => "proof_already_verified",
            Self::L1OriginTooOld => "l1_origin_too_old",
            Self::InvalidParentGame => "invalid_parent_game",
            Self::InvalidSigner => "invalid_signer",
            Self::TxReverted(_) => "tx_reverted",
            Self::TxManager(_) => "tx_manager",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_label_returns_stable_labels() {
        let cases = [
            (ProofSubmissionError::GameAlreadyExists, "game_already_exists"),
            (ProofSubmissionError::ProofAlreadyVerified, "proof_already_verified"),
            (ProofSubmissionError::L1OriginTooOld, "l1_origin_too_old"),
            (ProofSubmissionError::InvalidParentGame, "invalid_parent_game"),
            (ProofSubmissionError::InvalidSigner, "invalid_signer"),
            (ProofSubmissionError::TxReverted(B256::ZERO), "tx_reverted"),
            (ProofSubmissionError::TxManager(TxManagerError::NonceTooLow), "tx_manager"),
        ];

        for (error, label) in cases {
            assert_eq!(error.metric_label(), label);
        }
    }
}
