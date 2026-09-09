//! Errors returned by EIP-8130 authenticator dispatch.

use alloy_primitives::Address;

/// Reason an authentication blob was rejected during dispatch.
///
/// Every variant is a hard rejection: the transaction MUST NOT be admitted or
/// included on the strength of this authentication blob.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The blob was empty, too short, or otherwise structurally malformed for
    /// the routed authenticator (e.g. wrong fixed length, undecodable `WebAuthn`).
    #[error("authentication data is malformed")]
    MalformedAuth,

    /// The authenticator address is not one of the canonical EIP-8130
    /// authenticators (the pinned allowlist and the `K1_AUTHENTICATOR`
    /// sentinel), so it is not accepted on the block-validation path.
    #[error("authenticator {0} is not canonical")]
    NotCanonical(Address),

    /// The signature did not verify against the supplied hash / public key, or
    /// the verifying authenticator returned no actor (`bytes32(0)`).
    #[error("signature verification failed")]
    InvalidSignature,

    /// A delegate authentication tried to nest another delegate authenticator.
    /// Delegation is depth-1 only.
    #[error("delegate authentication cannot nest a delegate authenticator")]
    NestedDelegate,

    /// The supplied P-256 public-key coordinates do not lie on the curve.
    #[error("invalid public key")]
    InvalidPublicKey,
}
