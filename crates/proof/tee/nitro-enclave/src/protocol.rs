//! Typed request/response protocol for host ↔ enclave communication over vsock.

use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use serde::{Deserialize, Serialize};

/// Typed request sent from the host to the enclave over vsock.
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

/// Typed response returned by the enclave over vsock.
#[derive(Debug, Serialize, Deserialize)]
pub enum EnclaveResponse {
    /// Proof result for a [`EnclaveRequest::Prove`] request.
    Prove(Box<ProofResult>),
    /// 65-byte uncompressed ECDSA public key for [`EnclaveRequest::SignerPublicKey`].
    SignerPublicKey(Vec<u8>),
    /// Raw Nitro attestation document (`COSE_Sign1` bytes) for [`EnclaveRequest::SignerAttestation`].
    SignerAttestation(Vec<u8>),
    /// An error occurred while handling the request.
    Error(String),
}
