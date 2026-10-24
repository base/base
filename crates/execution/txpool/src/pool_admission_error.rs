//! Base-specific pool admission errors carried through [`PoolErrorKind::Other`].
//!
//! [`PoolErrorKind::Other`]: reth_transaction_pool::error::PoolErrorKind::Other

use reth_transaction_pool::error::{PoolError, PoolErrorKind};

/// Admission rejections Base adds on top of the protocol pool's error kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PoolAdmissionError {
    /// The transaction's sequential lane `(sender, nonce_key)` holds transactions of the other
    /// class (validity versus normal). Drain or cancel the lane before switching class; only a
    /// fee-bumped replacement of a lone same-nonce incumbent may switch it.
    #[error("nonce lane holds transactions of the other validity class")]
    LaneConflict,
    /// The validation is older than the account updates the validity store remembers, so its
    /// balance cannot be ordered against them. Resubmitting revalidates against fresher state.
    #[error("account state changed during validation; resubmit the transaction")]
    StaleAccountState,
}

impl PoolAdmissionError {
    /// Wraps the error for transaction `hash`.
    pub fn into_pool_error(self, hash: alloy_primitives::TxHash) -> PoolError {
        PoolError::other(hash, self)
    }

    /// Returns the admission error carried by `error`, if any.
    pub fn from_pool_error(error: &PoolError) -> Option<&Self> {
        match &error.kind {
            PoolErrorKind::Other(other) => other.downcast_ref(),
            _ => None,
        }
    }
}
