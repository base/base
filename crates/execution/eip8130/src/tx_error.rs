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
        scope: u8,
    },

    /// A config change or delegation targets a locked account. Both operations
    /// are rejected while locked. Mirrors `AccountConfiguration`'s
    /// `onlyUnlocked` modifier.
    #[error("account is locked")]
    AccountLocked,

    /// A delegation was not authorized by the account's native secp256k1 admin
    /// self actor.
    #[error("delegation requires the native-k1 admin self actor")]
    DelegationUnauthorized,

    /// A config change is bound to a chain other than `0` (multichain) or the
    /// local chain. Mirrors `require(chainId == 0 || chainId == block.chainid)`.
    #[error("config change chain id {got} is neither 0 nor the local chain {expected}")]
    ConfigChainId {
        /// The local chain id.
        expected: u64,
        /// The chain id carried by the config change.
        got: u64,
    },

    /// A config change's sequence does not match the account's current sequence
    /// for its channel. The contract reads the sequence from state, so a
    /// mismatch means the entry is stale or out of order (and its signed digest
    /// would not match the value that will actually be applied).
    #[error("config change sequence {got} does not match the expected {expected}")]
    ConfigSequence {
        /// The sequence read from the account's state for the entry's channel.
        expected: u64,
        /// The sequence carried by the config change.
        got: u64,
    },
}
