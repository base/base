//! Error types for TDX quote and collateral verification.

use alloy_primitives::Address;
use thiserror::Error;

/// Errors that can occur during TDX quote and collateral verification.
#[derive(Debug, Error)]
pub enum TdxVerifierError {
    /// ABI-encoded TDX verifier input was malformed.
    #[error("input decode error: {0}")]
    InputDecode(String),

    /// Raw TDX quote bytes were malformed or incomplete.
    #[error("invalid TDX quote: {0}")]
    InvalidQuote(String),

    /// TDX quote signature validation failed.
    #[error("TDX quote signature is invalid: {0}")]
    QuoteSignatureInvalid(String),

    /// Trusted Intel root CA hash did not match the provided root chain.
    #[error("trusted Intel root CA hash mismatch")]
    RootCaNotTrusted,

    /// PCK certificate chain validation failed.
    #[error("PCK certificate chain is invalid: {0}")]
    PckCertChainInvalid(String),

    /// TCB info collateral validation failed.
    #[error("TCB info collateral is invalid: {0}")]
    TcbInfoInvalid(String),

    /// QE identity collateral validation failed.
    #[error("QE identity collateral is invalid: {0}")]
    QeIdentityInvalid(String),

    /// Intel TCB status is not allowed by verifier policy.
    #[error("TCB status is not allowed")]
    TcbStatusNotAllowed,

    /// TD debug mode is not allowed.
    #[error("TDX debug mode is not allowed")]
    DebugTdNotAllowed,

    /// Required quote collateral is expired.
    #[error("TDX collateral is expired")]
    CollateralExpired,

    /// Quote timestamp is outside verifier policy.
    #[error("TDX quote timestamp is outside verifier policy")]
    InvalidTimestamp,

    /// Expected signer public key is malformed.
    #[error("expected secp256k1 public key is malformed")]
    MalformedPublicKey,

    /// TD report data does not bind the expected signer or registrar nonce.
    #[error("TD report data does not match expected signer binding")]
    ReportDataMismatch,

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
