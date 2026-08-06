//! Encoding of prover-service SP1 PLONK receipts into on-chain dispute proof bytes.

use alloy_primitives::{Bytes, hex};
use base_proof_primitives::ProofEncoder;
use sp1_sdk::{SP1Proof, SP1ProofWithPublicValues};
use thiserror::Error;

/// Error returned when a prover-service SP1 PLONK receipt cannot be decoded.
#[derive(Debug, Error)]
#[error("invalid SP1 PLONK receipt: {reason}")]
pub struct SnarkReceiptDecodeError {
    /// Why the receipt was rejected.
    pub reason: String,
}

/// Encodes prover-service SP1 PLONK receipts into the ZK dispute proof bytes
/// accepted by `verifyProposalProof` and `submitDispute` on chain.
#[derive(Debug, Clone, Copy)]
pub struct SnarkReceiptEncoder;

impl SnarkReceiptEncoder {
    /// Decodes a bincode-serialized [`SP1ProofWithPublicValues`] receipt and
    /// encodes it into on-chain ZK dispute proof bytes.
    ///
    /// The receipt bytes must come from a prover-service `snark_plonk` proof
    /// result. This is the single decode counterpart to the backends in
    /// `base-proof-zk-backend`, which serialize receipts with bincode's
    /// standard configuration.
    ///
    /// The receipt is validated instead of trusting
    /// [`SP1ProofWithPublicValues::bytes`], which panics on non-PLONK
    /// variants and malformed proof hex, and silently returns no bytes for
    /// mock proofs. Receipts carrying a TEE-2FA proof are rejected rather
    /// than replicating the `bytes()` behavior of prepending it to the
    /// seal: the prover-service SNARK path never sets one, so a receipt
    /// that does is unexpected and must not silently produce a seal in a
    /// format this codebase never verifies.
    pub fn encode_dispute_proof(receipt_bytes: &[u8]) -> Result<Bytes, SnarkReceiptDecodeError> {
        let invalid = |reason: String| SnarkReceiptDecodeError { reason };
        let (receipt, _): (SP1ProofWithPublicValues, usize) =
            bincode::serde::decode_from_slice(receipt_bytes, bincode::config::standard())
                .map_err(|error| invalid(format!("decoding receipt: {error}")))?;
        if receipt.tee_proof.is_some() {
            return Err(invalid(
                "receipt carries a TEE-2FA proof; TEE-prefixed seals are not supported".to_string(),
            ));
        }
        let SP1Proof::Plonk(plonk) = &receipt.proof else {
            return Err(invalid("receipt does not carry a PLONK proof".to_string()));
        };
        if plonk.encoded_proof.is_empty() {
            return Err(invalid(
                "receipt carries no proof bytes; mock proofs are not submittable".to_string(),
            ));
        }
        let proof = hex::decode(&plonk.encoded_proof)
            .map_err(|error| invalid(format!("decoding proof hex: {error}")))?;

        let mut bytes = Vec::with_capacity(4 + proof.len());
        bytes.extend_from_slice(&plonk.plonk_vkey_hash[..4]);
        bytes.extend_from_slice(&proof);
        Ok(ProofEncoder::encode_zk_dispute_proof_bytes(Bytes::from(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use sp1_sdk::SP1PublicValues;

    use super::*;

    fn plonk_receipt(encoded_proof: &str) -> SP1ProofWithPublicValues {
        let mut plonk_vkey_hash = [0u8; 32];
        plonk_vkey_hash[..4].copy_from_slice(&[0x5a, 0x09, 0x3a, 0x2f]);
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

    fn encode_receipt(receipt: &SP1ProofWithPublicValues) -> Vec<u8> {
        bincode::serde::encode_to_vec(receipt, bincode::config::standard()).unwrap()
    }

    #[test]
    fn encode_dispute_proof_prefixes_zk_type_and_vkey() {
        let proof =
            SnarkReceiptEncoder::encode_dispute_proof(&encode_receipt(&plonk_receipt("abcd")))
                .expect("valid receipt encodes");

        assert_eq!(proof.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
    }

    #[test]
    fn encode_dispute_proof_rejects_invalid_receipts() {
        SnarkReceiptEncoder::encode_dispute_proof(b"not-an-sp1-receipt")
            .expect_err("invalid receipt must fail");
    }

    #[test]
    fn encode_dispute_proof_rejects_non_plonk_receipts() {
        let receipt = SP1ProofWithPublicValues {
            proof: SP1Proof::Groth16(Default::default()),
            public_values: SP1PublicValues::new(),
            sp1_version: "test".to_owned(),
            tee_proof: None,
        };

        SnarkReceiptEncoder::encode_dispute_proof(&encode_receipt(&receipt))
            .expect_err("non-PLONK receipt must fail");
    }

    #[test]
    fn encode_dispute_proof_rejects_tee_2fa_receipts() {
        let mut receipt = plonk_receipt("abcd");
        receipt.tee_proof = Some(vec![0xEE; 8]);

        SnarkReceiptEncoder::encode_dispute_proof(&encode_receipt(&receipt))
            .expect_err("TEE-2FA receipt must fail");
    }

    #[test]
    fn encode_dispute_proof_rejects_mock_proofs() {
        SnarkReceiptEncoder::encode_dispute_proof(&encode_receipt(&plonk_receipt("")))
            .expect_err("mock receipt must fail");
    }

    #[test]
    fn encode_dispute_proof_rejects_malformed_proof_hex() {
        SnarkReceiptEncoder::encode_dispute_proof(&encode_receipt(&plonk_receipt("zzzz")))
            .expect_err("malformed proof hex must fail");
    }
}
