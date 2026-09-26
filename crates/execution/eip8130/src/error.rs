//! Errors returned by EIP-8130 secp256k1 authentication.

use alloy_primitives::Address;

/// Reason an authentication blob was rejected.
///
/// Every variant is a hard rejection: the transaction MUST NOT be admitted or
/// included on the strength of this authentication blob.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The blob was empty, too short, or otherwise structurally malformed (e.g.
    /// wrong fixed length for a `authenticator(20) || r||s||v(65)` blob).
    #[error("authentication data is malformed")]
    MalformedAuth,

    /// The authenticator selector is not the canonical native secp256k1
    /// sentinel (`K1_AUTHENTICATOR`), the only authenticator accepted on the
    /// launch wire.
    #[error("authenticator {0} is not canonical")]
    NotCanonical(Address),

    /// The signature did not verify against the supplied hash (recovery failed
    /// or the signature is malleable / non-canonical).
    #[error("signature verification failed")]
    InvalidSignature,
}
