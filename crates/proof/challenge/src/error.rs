//! Challenge submission error types.

use alloy_primitives::B256;
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
    /// An internal asynchronous transaction task panicked.
    #[error("bond transaction task panicked: {message}")]
    BondTaskPanicked {
        /// Panic message captured from the asynchronous task.
        message: String,
    },
    /// Transaction manager error (nonce, fees, RPC, signing, etc.).
    #[error(transparent)]
    TxManager(#[from] TxManagerError),
}
