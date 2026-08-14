//! Typed request/response protocol for host ↔ enclave communication over vsock.

use alloy_primitives::{B256, Bytes};
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
    /// Verify a withdrawal record and sign its authorization hash.
    SignAttestedWithdrawal {
        /// Hash of the L2 withdrawal authorization fields.
        auth_hash: B256,
        /// L2-to-L1 message passer storage root supplied by the relayer.
        message_passer_storage_root: B256,
        /// Secure-trie proof for the withdrawal record.
        storage_proof: Vec<Bytes>,
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
    /// Raw ECDSA signature for a [`EnclaveRequest::SignAttestedWithdrawal`] request.
    AttestedWithdrawal(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_existing_request_variant_indices() {
        let signer_public_key = bincode::serde::encode_to_vec(
            &EnclaveRequest::SignerPublicKey,
            bincode::config::standard(),
        )
        .unwrap();
        let signer_attestation = bincode::serde::encode_to_vec(
            &EnclaveRequest::SignerAttestation { user_data: None, nonce: None },
            bincode::config::standard(),
        )
        .unwrap();
        let attested_withdrawal = bincode::serde::encode_to_vec(
            &EnclaveRequest::SignAttestedWithdrawal {
                auth_hash: B256::ZERO,
                message_passer_storage_root: B256::ZERO,
                storage_proof: vec![],
            },
            bincode::config::standard(),
        )
        .unwrap();

        assert_eq!(signer_public_key[0], 1);
        assert_eq!(signer_attestation[0], 2);
        assert_eq!(attested_withdrawal[0], 3);
    }

    #[test]
    fn preserves_existing_response_variant_indices() {
        let signer_public_key = bincode::serde::encode_to_vec(
            &EnclaveResponse::SignerPublicKey(vec![]),
            bincode::config::standard(),
        )
        .unwrap();
        let signer_attestation = bincode::serde::encode_to_vec(
            &EnclaveResponse::SignerAttestation(vec![]),
            bincode::config::standard(),
        )
        .unwrap();
        let error = bincode::serde::encode_to_vec(
            &EnclaveResponse::Error(String::new()),
            bincode::config::standard(),
        )
        .unwrap();
        let attested_withdrawal = bincode::serde::encode_to_vec(
            &EnclaveResponse::AttestedWithdrawal(vec![]),
            bincode::config::standard(),
        )
        .unwrap();

        assert_eq!(signer_public_key[0], 1);
        assert_eq!(signer_attestation[0], 2);
        assert_eq!(error[0], 3);
        assert_eq!(attested_withdrawal[0], 4);
    }
}
