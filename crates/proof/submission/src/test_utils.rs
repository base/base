//! Test fixtures for consumers of the SNARK receipt encoding.

use sp1_sdk::{SP1Proof, SP1ProofWithPublicValues, SP1PublicValues};

/// Builds prover-service SP1 PLONK receipt fixtures for tests.
///
/// Consumers of [`crate::SnarkReceiptEncoder`] can use this instead of
/// depending on `sp1-sdk` and `bincode` directly to fabricate receipt bytes.
#[derive(Debug, Clone, Copy)]
pub struct SnarkReceiptFixture;

impl SnarkReceiptFixture {
    /// Returns an SP1 receipt carrying a PLONK proof with the given
    /// verifying-key prefix and hex-encoded proof.
    pub fn plonk_receipt(vkey_prefix: [u8; 4], encoded_proof: &str) -> SP1ProofWithPublicValues {
        let mut plonk_vkey_hash = [0u8; 32];
        plonk_vkey_hash[..4].copy_from_slice(&vkey_prefix);
        let mut receipt = SP1ProofWithPublicValues {
            proof: SP1Proof::Plonk(Default::default()),
            public_values: SP1PublicValues::new(),
            sp1_version: "test".to_owned(),
            tee_proof: None,
        };
        let SP1Proof::Plonk(plonk) = &mut receipt.proof else {
            unreachable!();
        };
        plonk.encoded_proof = encoded_proof.to_owned();
        plonk.plonk_vkey_hash = plonk_vkey_hash;
        receipt
    }

    /// Bincode-encodes a receipt with the prover-service wire configuration
    /// used by the `base-proof-zk-backend` provers.
    pub fn receipt_bytes(receipt: &SP1ProofWithPublicValues) -> Vec<u8> {
        bincode::serde::encode_to_vec(receipt, bincode::config::standard())
            .expect("receipt fixture must encode")
    }

    /// Returns bincode-encoded receipt bytes carrying a PLONK proof with the
    /// given verifying-key prefix and hex-encoded proof, matching the
    /// payloads produced by the `base-proof-zk-backend` provers.
    pub fn plonk_receipt_bytes(vkey_prefix: [u8; 4], encoded_proof: &str) -> Vec<u8> {
        Self::receipt_bytes(&Self::plonk_receipt(vkey_prefix, encoded_proof))
    }
}
