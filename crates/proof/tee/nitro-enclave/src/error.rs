//! Error types for enclave server operations.

use base_proof_preimage::PreimageKey;
use thiserror::Error;

/// Errors that can occur during NSM operations.
#[derive(Debug, Clone, Error)]
pub enum NsmError {
    /// Failed to open NSM session.
    #[error("failed to open NSM session: {0}")]
    SessionOpen(String),
    /// NSM device returned an error.
    #[error("NSM device error: {0}")]
    DeviceError(String),
    /// Failed to describe PCR.
    #[error("failed to describe PCR: {0}")]
    DescribePcr(String),
    /// NSM device did not return PCR data.
    #[error("NSM device did not return PCR data")]
    NoPcrData,
    /// Failed to get attestation.
    #[error("failed to get attestation: {0}")]
    Attestation(String),
    /// NSM device did not return an attestation.
    #[error("NSM device did not return an attestation")]
    NoAttestation,
    /// Failed to get random bytes.
    #[error("failed to get random bytes: {0}")]
    Random(String),
}

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Error)]
pub enum CryptoError {
    /// Failed to parse ECDSA private key.
    #[error("failed to parse ECDSA private key: {0}")]
    EcdsaKeyParse(String),
    /// Failed to parse hex string.
    #[error("failed to parse hex string: {0}")]
    HexParse(String),
}

/// Errors that can occur during proposal operations.
#[derive(Debug, Clone, Error)]
pub enum ProposalError {
    /// No proposals provided for aggregation.
    #[error("no proposals provided for aggregation")]
    EmptyProposals,
    /// Intermediate block interval must not be zero.
    #[error("intermediate_block_interval must not be zero")]
    InvalidInterval,
    /// Signature verification failed at the given index.
    #[error("invalid signature at proposal index {index}: {reason}")]
    InvalidSignature {
        /// Index of the proposal with invalid signature.
        index: usize,
        /// Reason for the failure.
        reason: String,
    },
    /// Signature is not the expected 65 bytes.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),
    /// ECDSA signing failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),
    /// Public key recovery failed.
    #[error("public key recovery failed: {0}")]
    RecoveryFailed(String),
    /// Intermediate root does not match the proposal's output root at that block.
    #[error("intermediate root mismatch at index {index}: expected {expected}, got {actual}")]
    InvalidIntermediateRoot {
        /// Index of the mismatched intermediate root.
        index: usize,
        /// The output root from the proposal at that block.
        expected: String,
        /// The intermediate root provided by the caller.
        actual: String,
    },
    /// No proposal found for an intermediate root at the given block.
    #[error("no proposal found for intermediate root at block {block}")]
    MissingIntermediateProposal {
        /// The block number where a proposal was expected.
        block: u64,
    },
    /// Core execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

/// Top-level error type for nitro enclave operations.
#[derive(Debug, Clone, Error)]
pub enum NitroError {
    /// NSM error.
    #[error(transparent)]
    Nsm(#[from] NsmError),
    /// Cryptographic error.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Proposal error.
    #[error(transparent)]
    Proposal(#[from] ProposalError),
    /// Environment variable error.
    #[error("environment variable error: {0}")]
    EnvVar(String),
    /// Proof pipeline error.
    #[error("proof pipeline error: {0}")]
    ProofPipeline(String),
    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
    /// Unsupported chain ID.
    #[error("unsupported chain ID: {0}")]
    UnsupportedChain(u64),
    /// A preimage's content does not match its hash-based key.
    #[error("preimage hash mismatch for key {0}")]
    InvalidPreimage(PreimageKey),
}

/// A specialized Result type for nitro enclave operations.
pub type Result<T> = std::result::Result<T, NitroError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn error_types_are_send_sync() {
        assert_send_sync::<NsmError>();
        assert_send_sync::<CryptoError>();
        assert_send_sync::<ProposalError>();
        assert_send_sync::<NitroError>();
    }

    #[test]
    fn error_types_are_clone() {
        fn assert_clone<T: Clone>() {}

        assert_clone::<NsmError>();
        assert_clone::<CryptoError>();
        assert_clone::<ProposalError>();
        assert_clone::<NitroError>();
    }
}
