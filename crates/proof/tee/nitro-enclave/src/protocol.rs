//! Typed request and response messages of the Enclave link.

use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Typed request sent from the host to the enclave.
#[derive(Debug, Serialize, Deserialize)]
pub enum EnclaveRequest {
    /// Run the proof pipeline with the given witness preimages.
    Prove(Vec<(PreimageKey, Vec<u8>)>),
    /// Return the enclave's 65-byte uncompressed ECDSA public key.
    SignerPublicKey,
    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes).
    SignerAttestation {
        /// Optional application-specific data to bind into the attestation.
        user_data: Option<Vec<u8>>,
        /// Optional nonce to bind into the attestation for replay protection.
        nonce: Option<Vec<u8>>,
    },
}

impl EnclaveRequest {
    /// Maximum `user_data` length accepted by the NSM attestation API.
    pub const MAX_USER_DATA_BYTES: usize = 512;

    /// Maximum `nonce` length accepted by the NSM attestation API.
    pub const MAX_NONCE_BYTES: usize = 512;

    /// Operation name used in logs and errors.
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::Prove(_) => "prove",
            Self::SignerPublicKey => "signer_public_key",
            Self::SignerAttestation { .. } => "signer_attestation",
        }
    }

    /// Checks the request against the limits the enclave enforces.
    pub fn validate(&self) -> Result<(), EnclaveFailure> {
        if let Self::SignerAttestation { user_data, nonce } = self {
            if user_data.as_ref().is_some_and(|data| data.len() > Self::MAX_USER_DATA_BYTES) {
                return Err(EnclaveFailure::InvalidRequest(format!(
                    "user_data exceeds {}-byte limit",
                    Self::MAX_USER_DATA_BYTES
                )));
            }
            if nonce.as_ref().is_some_and(|nonce| nonce.len() > Self::MAX_NONCE_BYTES) {
                return Err(EnclaveFailure::InvalidRequest(format!(
                    "nonce exceeds {}-byte limit",
                    Self::MAX_NONCE_BYTES
                )));
            }
        }
        Ok(())
    }
}

/// Typed response returned by the enclave.
///
/// Variant order is part of the bincode wire format: append new variants at the end.
#[derive(Debug, Serialize, Deserialize)]
pub enum EnclaveResponse {
    /// Proof result for a [`EnclaveRequest::Prove`] request.
    Prove(Box<ProofResult>),
    /// 65-byte uncompressed ECDSA public key for [`EnclaveRequest::SignerPublicKey`].
    SignerPublicKey(Vec<u8>),
    /// Raw Nitro attestation document (`COSE_Sign1` bytes) for [`EnclaveRequest::SignerAttestation`].
    SignerAttestation(Vec<u8>),
    /// Untyped failure sent by enclave images that predate [`EnclaveResponse::Failure`].
    Error(String),
    /// The enclave refused or failed the request.
    Failure(EnclaveFailure),
}

/// An Enclave rejection: the enclave refused or failed a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum EnclaveFailure {
    /// The request breaks a limit the enclave enforces.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The proof pipeline failed for the given witness.
    #[error("proof failed: {0}")]
    Proof(String),
    /// The NSM could not produce an attestation document.
    #[error("attestation failed: {0}")]
    Attestation(String),
}
