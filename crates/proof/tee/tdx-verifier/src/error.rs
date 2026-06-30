//! Error types for TDX quote and collateral verification.

use thiserror::Error;

use crate::TDXVerificationResult;

/// Errors that can occur during TDX quote and collateral verification.
#[derive(Debug, Clone, Error)]
pub enum TdxVerifierError {
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

    /// Required quote collateral is expired.
    #[error("TDX collateral is expired")]
    CollateralExpired,

    /// Quote timestamp is outside verifier policy.
    #[error("TDX quote timestamp is outside verifier policy")]
    InvalidTimestamp,

    /// Expected signer public key is malformed.
    #[error("expected secp256k1 public key is malformed")]
    MalformedPublicKey,

    /// Expected signer address does not match the public key.
    #[error("expected signer does not match public key")]
    SignerMismatch,

    /// TD report data does not bind the expected public key.
    #[error("TD report data does not match expected public key binding")]
    ReportDataMismatch,
}

impl TdxVerifierError {
    /// Returns the same error class with a replaced diagnostic message when supported.
    pub fn with_message(self, message: String) -> Self {
        match self {
            Self::InvalidQuote(_) => Self::InvalidQuote(message),
            Self::QuoteSignatureInvalid(_) => Self::QuoteSignatureInvalid(message),
            Self::PckCertChainInvalid(_) => Self::PckCertChainInvalid(message),
            Self::TcbInfoInvalid(_) => Self::TcbInfoInvalid(message),
            Self::QeIdentityInvalid(_) => Self::QeIdentityInvalid(message),
            other => other,
        }
    }

    /// Returns the contract result enum corresponding to this error.
    pub const fn result(&self) -> TDXVerificationResult {
        match self {
            Self::InvalidQuote(_) => TDXVerificationResult::InvalidQuote,
            Self::QuoteSignatureInvalid(_) => TDXVerificationResult::QuoteSignatureInvalid,
            Self::RootCaNotTrusted => TDXVerificationResult::RootCaNotTrusted,
            Self::PckCertChainInvalid(_) => TDXVerificationResult::PckCertChainInvalid,
            Self::TcbInfoInvalid(_) => TDXVerificationResult::TcbInfoInvalid,
            Self::QeIdentityInvalid(_) => TDXVerificationResult::QeIdentityInvalid,
            Self::TcbStatusNotAllowed => TDXVerificationResult::TcbStatusNotAllowed,
            Self::CollateralExpired => TDXVerificationResult::CollateralExpired,
            Self::InvalidTimestamp => TDXVerificationResult::InvalidTimestamp,
            Self::MalformedPublicKey | Self::SignerMismatch | Self::ReportDataMismatch => {
                TDXVerificationResult::ReportDataMismatch
            }
        }
    }
}

/// Convenience result alias for TDX verifier operations.
pub type Result<T> = std::result::Result<T, TdxVerifierError>;
