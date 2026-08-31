//! Errors returned by transaction actor authorization.

use crate::{ApplyError, AuthorizeError, Operation};

/// Reason a transaction's actors could not be authorized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxAuthError {
    /// The sender or payer actor failed the stateful authorize step (dispatch,
    /// binding, expiry, implicit-EOA rule, or a storage read).
    #[error("actor authorization failed: {0}")]
    Authorize(#[from] AuthorizeError),

    /// Applying an account change to the (evolving) state failed. Account
    /// changes are authorized and applied interleaved against the working state,
    /// so an apply-step `require`/`revert` (a bad create address, an unsorted
    /// initial-actor set, a malformed actor change, a sequence overflow, …)
    /// surfaces here as a transaction-authorization failure.
    #[error("account change apply failed: {0}")]
    Apply(#[from] ApplyError),

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
        scope: u16,
    },

    /// A config change or delegation targets a locked account in a way the
    /// keystore rejects (`RevokeActor`, delegation). `AuthorizeActor` while locked
    /// is permitted when the grant outlives the unlock floor; see
    /// [`crate::AccountChangeApplier`]. Mirrors `Keystore.AccountIsLocked`.
    #[error("account is locked")]
    AccountLocked,

    /// A delegation was not authorized by an admin (unrestricted) actor on the
    /// unlocked account.
    #[error("delegation requires an admin actor")]
    DelegationUnauthorized,

    /// A signed account-change batch's sequence does not match the account's
    /// current sequence for its channel. The contract reads the sequence from
    /// state, so a mismatch means the batch is stale or out of order (and its
    /// signed digest would not match the value that will actually be applied).
    /// Mirrors `Keystore.BadSequence`.
    #[error("config change sequence {got} does not match the expected {expected}")]
    ConfigSequence {
        /// The sequence read from the account's state for the batch's channel.
        expected: u64,
        /// The sequence carried by the signed account-change batch.
        got: u64,
    },

    /// A Local-channel batch's committed `localEpoch` (the high half of its
    /// `sequence` word) does not match the account's current local epoch. An
    /// [`crate::AccountChangeApplier`]-applied `IncrementLocalEpoch` advances the
    /// epoch, retiring every unlanded local signature at a prior epoch. Mirrors
    /// `Keystore.StaleEpoch`.
    #[error("config change local epoch {got} does not match the expected {expected}")]
    StaleEpoch {
        /// The account's current local epoch.
        expected: u64,
        /// The local epoch committed by the signed account-change batch.
        got: u64,
    },

    /// A channel's sequence counter is at its terminal value and cannot advance.
    /// Mirrors `Keystore.SequenceSaturated`.
    #[error("account-change channel sequence is saturated")]
    SequenceSaturated,
}
