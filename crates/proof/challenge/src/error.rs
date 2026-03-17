//! Challenge submission error types.

use alloy_primitives::B256;
use base_tx_manager::TxManagerError;
use thiserror::Error;

/// Errors that can occur when submitting a challenge transaction.
#[derive(Debug, Error)]
pub enum ChallengeSubmitError {
    /// The nullify transaction was mined but reverted on-chain.
    #[error("nullify transaction reverted: {tx_hash}")]
    TxReverted {
        /// Hash of the reverted transaction.
        tx_hash: B256,
    },
    /// Transaction manager error (nonce, fees, RPC, signing, etc.).
    #[error(transparent)]
    TxManager(#[from] TxManagerError),
}
