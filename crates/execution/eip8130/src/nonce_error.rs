//! Errors returned by the EIP-8130 nonce validation step.

use base_precompile_storage::BasePrecompileError;

/// Reason a transaction's nonce could not be validated against the live nonce
/// state. Every variant is a hard rejection for block inclusion; in the mempool
/// a sequence *ahead* of the channel is not an error (see
/// [`NonceStatus::Buffered`](crate::NonceStatus)).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NonceError {
    /// The transaction's `nonce_sequence` is below the channel's current nonce,
    /// so it has already been used. Stale in every mode.
    #[error("nonce sequence {got} is below the channel nonce {channel}")]
    TooLow {
        /// The channel's current nonce read from state.
        channel: u64,
        /// The sequence carried by the transaction.
        got: u64,
    },

    /// The transaction's `nonce_sequence` is ahead of the channel's current
    /// nonce, leaving a gap. Only an error under [`NonceMode::Inclusion`](crate::NonceMode);
    /// the pool buffers such transactions instead.
    #[error("nonce sequence {got} is ahead of the channel nonce {channel}")]
    TooHigh {
        /// The channel's current nonce read from state.
        channel: u64,
        /// The sequence carried by the transaction.
        got: u64,
    },

    /// A nonce-free (`NONCE_KEY_MAX`) transaction's replay hash is already
    /// recorded and has not yet expired. Mirrors the nonce manager's
    /// `ExpiringNonceReplay`.
    #[error("nonce-free replay hash already recorded")]
    Replay,

    /// A read against the nonce manager precompile storage failed.
    #[error("nonce-manager read failed: {0}")]
    Storage(#[from] BasePrecompileError),
}
