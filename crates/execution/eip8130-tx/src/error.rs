//! Errors returned by transaction actor authorization.

use base_execution_eip8130_authorize::AuthorizeError;

use crate::Operation;

/// Reason a transaction's actors could not be authorized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxAuthError {
    /// The sender or payer actor failed the stateful authorize step (dispatch,
    /// binding, expiry, implicit-EOA rule, or a storage read).
    #[error("actor authorization failed: {0}")]
    Authorize(#[from] AuthorizeError),

    /// The EOA-path sender signature was malformed or did not recover (wrong
    /// length, or a non-canonical upper-half `s` rejected by the checked
    /// recovery). Applies only when `tx.sender` is `None`.
    #[error("EOA sender signature could not be recovered")]
    SenderRecovery,

    /// The resolved actor is valid but its scope does not grant the operation it
    /// was authorized for. Mirrors the contract's scope requirement.
    #[error("{operation:?} actor scope {scope:#04x} does not grant the required context")]
    Scope {
        /// The operation whose scope requirement was not met.
        operation: Operation,
        /// The resolved actor's scope bitfield.
        scope: u8,
    },
}
