//! Error types for the `payer_*` JSON-RPC surface.

use alloy_primitives::Address;
use base_execution_payer::PayerSignerError;
use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};

/// A failure surfaced by a [`PayerTerms`](crate::PayerTerms) resolver while
/// producing the per-block price snapshot (e.g. a missing state provider or a
/// storage-decode error). Kept opaque so the concrete node/builder wiring can
/// carry its own error without this crate depending on it.
#[derive(Debug, thiserror::Error)]
#[error("failed to resolve payer terms: {0}")]
pub struct PayerTermsError(pub String);

impl PayerTermsError {
    /// Wraps any displayable error as a [`PayerTermsError`].
    pub fn new(error: impl core::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

/// Errors returned by the `payer_*` handlers.
#[derive(Debug, thiserror::Error)]
pub enum PayerRpcError {
    /// The submitted bytes are not a decodable EIP-2718 transaction.
    #[error("failed to decode transaction: {0}")]
    Decode(String),

    /// The submitted transaction is not an EIP-8130 transaction, so it cannot
    /// be sponsored by the payer service.
    #[error("transaction is not an EIP-8130 transaction")]
    NotEip8130,

    /// The transaction's `payer` field does not designate this builder's payer
    /// account, so co-signing it would produce an unusable `payer_auth`.
    #[error("transaction payer {found:?} does not designate the builder payer {expected}")]
    PayerMismatch {
        /// The `payer` the transaction designates.
        found: Option<Address>,
        /// This builder's payer account.
        expected: Address,
    },

    /// The transaction already carries a `payer_auth`; the payer service only
    /// co-signs transactions whose payer authorization is still empty.
    #[error("transaction already carries a payer_auth")]
    AlreadyCosigned,

    /// The sender could not be recovered from the transaction's `sender_auth`.
    #[error("failed to recover transaction sender")]
    SenderRecovery,

    /// The payer service is configured but not currently accepting
    /// transactions.
    #[error("payer service is not currently enabled")]
    Disabled,

    /// The on-chain configured payer does not match this builder's payer key,
    /// so this node cannot co-sign for it.
    #[error("configured payer {configured} does not match this builder's payer {actual}")]
    PayerNotConfigured {
        /// The payer recorded in the on-chain config snapshot.
        configured: Address,
        /// This builder's payer account.
        actual: Address,
    },

    /// No accepted token is currently quotable, so the service has nothing to
    /// sponsor against.
    #[error("no accepted token is currently quotable")]
    NoQuotableTokens,

    /// Co-signing the payer digest failed.
    #[error(transparent)]
    Signer(#[from] PayerSignerError),

    /// Resolving the per-block payer terms failed.
    #[error(transparent)]
    Terms(#[from] PayerTermsError),

    /// The transaction pool rejected the co-signed transaction.
    #[error("pool rejected transaction: {0}")]
    Pool(String),
}

impl PayerRpcError {
    /// The JSON-RPC error code this failure maps to: caller-fixable input
    /// errors are `InvalidParams`, everything else is a server-side
    /// `InternalError`.
    const fn code(&self) -> ErrorCode {
        match self {
            Self::Decode(_)
            | Self::NotEip8130
            | Self::PayerMismatch { .. }
            | Self::AlreadyCosigned
            | Self::SenderRecovery => ErrorCode::InvalidParams,
            Self::Disabled
            | Self::PayerNotConfigured { .. }
            | Self::NoQuotableTokens
            | Self::Signer(_)
            | Self::Terms(_)
            | Self::Pool(_) => ErrorCode::InternalError,
        }
    }
}

impl From<PayerRpcError> for ErrorObjectOwned {
    fn from(error: PayerRpcError) -> Self {
        Self::owned(error.code().code(), error.to_string(), None::<()>)
    }
}
