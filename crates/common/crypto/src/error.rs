//! Error type for the shared elliptic-curve signature primitives.

/// Reason a signature primitive rejected its inputs.
///
/// Every variant is a hard rejection: nothing may be authenticated on the
/// strength of an input that produced one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// The signature payload was the wrong length for the routed primitive.
    #[error("signature payload is malformed")]
    MalformedSignature,

    /// The signature failed to parse, was malleable (upper-half `s`), or did not
    /// verify / recover against the supplied prehash and key.
    #[error("signature verification failed")]
    InvalidSignature,

    /// The public key could not be decoded or does not lie on the curve.
    #[error("invalid public key")]
    InvalidPublicKey,
}
