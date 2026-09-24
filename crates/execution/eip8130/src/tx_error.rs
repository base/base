//! Errors returned by transaction actor authorization.

use alloy_primitives::Address;

use crate::{ApplyError, AuthError};

/// Reason a transaction's actors could not be authorized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxAuthError {
    /// Applying a delegation account change failed (the account carries
    /// non-delegatable code, or a storage access failed).
    #[error("account change apply failed: {0}")]
    Apply(#[from] ApplyError),

    /// A named sender or payer `sender_auth`/`payer_auth` blob was malformed, did
    /// not carry the canonical secp256k1 authenticator, or its signature did not
    /// recover.
    #[error("actor authentication failed: {0}")]
    Authenticate(#[from] AuthError),

    /// The EOA-path sender signature was malformed or did not recover (wrong
    /// length, or a non-canonical upper-half `s` rejected by the checked
    /// recovery). Applies only when `tx.sender` is `None`.
    #[error("EOA sender signature could not be recovered")]
    SenderRecovery,

    /// The open-payer `payer_auth` was malformed or did not recover. Applies
    /// only in open payer mode.
    #[error("open payer signature could not be recovered")]
    PayerRecovery,

    /// A named sender's `sender_auth` recovered to a signer that is not the named
    /// account.
    #[error("sender auth recovered {recovered} but the named sender is {expected}")]
    SenderMismatch {
        /// The named sender account (`tx.sender`).
        expected: Address,
        /// The address the sender signature recovered to.
        recovered: Address,
    },

    /// A named payer's `payer_auth` recovered to a signer that is not the named
    /// payer account.
    #[error("payer auth recovered {recovered} but the named payer is {expected}")]
    PayerMismatch {
        /// The named payer account (`tx.payer`).
        expected: Address,
        /// The address the payer signature recovered to.
        recovered: Address,
    },
}
