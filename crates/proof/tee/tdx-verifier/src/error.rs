//! Error types for Confidential Space TDX token verification.

use alloy_primitives::Address;
use thiserror::Error;

/// Errors that can occur during Confidential Space token verification.
#[derive(Debug, Error)]
pub enum TdxVerifierError {
    /// ABI-encoded TDX verifier input was malformed.
    #[error("input decode error: {0}")]
    InputDecode(String),

    /// Confidential Space token was malformed.
    #[error("Confidential Space token is malformed: {0}")]
    TokenMalformed(String),

    /// Confidential Space token signature validation failed.
    #[error("Confidential Space token signature is invalid: {0}")]
    TokenSignatureInvalid(String),

    /// Trusted Confidential Space root CA hash did not match the token chain.
    #[error("trusted Confidential Space root CA hash mismatch")]
    RootCaNotTrusted,

    /// Confidential Space token claims did not satisfy policy.
    #[error("Confidential Space token claims are not allowed")]
    TokenClaimsInvalid,

    /// Token issuance or expiration time is outside verifier policy.
    #[error("Confidential Space token time is outside verifier policy")]
    TokenExpired,

    /// Expected signer public key is malformed.
    #[error("expected secp256k1 public key is malformed")]
    MalformedPublicKey,

    /// Token nonce does not bind the expected signer and registrar context.
    #[error("Confidential Space token nonce does not match expected signer binding")]
    TokenNonceMismatch,

    /// Decoded input signer does not match the signer being registered.
    #[error("signer mismatch: expected {expected}, got {actual}")]
    SignerMismatch {
        /// Signer supplied by the registrar.
        expected: Address,
        /// Signer committed by the TDX verifier input.
        actual: Address,
    },
}

/// Convenience result alias for TDX verifier operations.
pub type Result<T> = std::result::Result<T, TdxVerifierError>;
