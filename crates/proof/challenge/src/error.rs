//! Challenge submission error types.

use alloy_primitives::B256;
use base_proof_submission::KnownRevert;
use base_tx_manager::TxManagerError;
use thiserror::Error;

/// Errors that can occur when submitting a challenge transaction.
#[derive(Debug, Error)]
pub enum ChallengeSubmitError {
    /// A transaction was mined but reverted on-chain.
    #[error("transaction reverted: {tx_hash}")]
    TxReverted {
        /// Hash of the reverted transaction.
        tx_hash: B256,
    },
    /// A known proof-contract custom error was decoded from the revert.
    #[error(transparent)]
    KnownRevert(#[from] KnownRevert),
    /// Transaction manager error (nonce, fees, RPC, signing, etc.).
    #[error(transparent)]
    TxManager(TxManagerError),
}

impl From<TxManagerError> for ChallengeSubmitError {
    fn from(err: TxManagerError) -> Self {
        if let Some(revert) = KnownRevert::from_tx_manager_error(&err) {
            return Self::KnownRevert(revert);
        }

        Self::TxManager(err)
    }
}
