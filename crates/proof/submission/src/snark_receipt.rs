//! Encoding of prover-service SP1 PLONK receipts into on-chain dispute proof bytes.

use alloy_primitives::{Bytes, hex};
use base_proof_primitives::ProofEncoder;
use bincode::error::{AllowedEnumVariants, DecodeError};
use thiserror::Error;

/// Error returned when a prover-service SP1 PLONK receipt cannot be decoded.
#[derive(Debug, Error)]
pub enum SnarkReceiptDecodeError {
    /// The receipt bytes are not a bincode-encoded SP1 receipt.
    #[error("decoding receipt: {0}")]
    Receipt(#[from] DecodeError),

    /// The receipt carries a TEE-2FA proof.
    #[error("receipt carries a TEE-2FA proof; TEE-prefixed seals are not supported")]
    TeeProof,

    /// The receipt does not carry a PLONK proof.
    #[error("receipt does not carry a PLONK proof")]
    NotPlonk,

    /// The receipt carries no proof bytes (mock proof).
    #[error("receipt carries no proof bytes; mock proofs are not submittable")]
    MockProof,

    /// The PLONK proof hex is malformed.
    #[error("decoding proof hex: {0}")]
    ProofHex(#[from] hex::FromHexError),
}

/// Encodes prover-service SP1 PLONK receipts into the ZK dispute proof bytes
/// accepted by `verifyProposalProof` and `submitDispute` on chain.
#[derive(Debug, Clone, Copy)]
pub struct SnarkReceiptEncoder;

/// Bincode variant index of `SP1Proof::Plonk` in SP1's
/// `Core | Compressed | Plonk | Groth16` proof enum.
const SP1_PLONK_VARIANT: u32 = 2;

/// Variant indices of SP1's `Core | Compressed | Plonk | Groth16` proof enum.
const SP1_PROOF_VARIANTS: AllowedEnumVariants = AllowedEnumVariants::Range { min: 0, max: 3 };

/// Wire layout of an SP1 receipt after its proof-variant index, when the
/// variant is PLONK: the PLONK proof fields (`public_inputs`, `encoded_proof`,
/// `raw_proof`, `plonk_vkey_hash`), then the receipt's `public_values`,
/// `sp1_version`, and `tee_proof`.
type PlonkReceiptFields = ([String; 5], String, String, [u8; 32], Vec<u8>, String, Option<Vec<u8>>);

impl SnarkReceiptEncoder {
    /// Decodes a bincode-serialized SP1 `SP1ProofWithPublicValues` receipt and
    /// encodes it into on-chain ZK dispute proof bytes.
    ///
    /// The receipt bytes must come from a prover-service `snark_plonk` proof
    /// result. This is the single decode counterpart to the backends in
    /// `base-proof-zk-backend`, which serialize receipts with bincode's
    /// standard configuration.
    ///
    /// The receipt is decoded from its wire layout instead of through
    /// `sp1-sdk`, which would link the whole SP1 prover stack. Tests encode
    /// fixtures with the pinned `sp1-sdk` types, so a layout change fails
    /// them. Receipts carrying a TEE-2FA proof are rejected: the
    /// prover-service SNARK path never sets one, so a receipt that does is
    /// unexpected and must not silently produce a seal in a format this
    /// codebase never verifies. Mock proofs (no proof bytes) are rejected too.
    pub fn encode_onchain_zk_proof(receipt_bytes: &[u8]) -> Result<Bytes, SnarkReceiptDecodeError> {
        let config = bincode::config::standard();
        let (variant, offset): (u32, usize) =
            bincode::serde::decode_from_slice(receipt_bytes, config)?;
        match variant {
            SP1_PLONK_VARIANT => {}
            0..=3 => return Err(SnarkReceiptDecodeError::NotPlonk),
            found => {
                return Err(DecodeError::UnexpectedVariant {
                    type_name: "SP1Proof",
                    allowed: &SP1_PROOF_VARIANTS,
                    found,
                }
                .into());
            }
        }
        let ((_, encoded_proof, _, plonk_vkey_hash, _, _, tee_proof), _): (PlonkReceiptFields, _) =
            bincode::serde::decode_from_slice(&receipt_bytes[offset..], config)?;
        if tee_proof.is_some() {
            return Err(SnarkReceiptDecodeError::TeeProof);
        }
        if encoded_proof.is_empty() {
            return Err(SnarkReceiptDecodeError::MockProof);
        }
        let proof = hex::decode(&encoded_proof)?;

        let bytes = [&plonk_vkey_hash[..4], &proof].concat();
        Ok(ProofEncoder::encode_zk_dispute_proof_bytes(Bytes::from(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use sp1_sdk::{SP1Proof, SP1ProofWithPublicValues, SP1PublicValues};

    use super::*;
    use crate::test_utils::SnarkReceiptFixture;

    /// Arbitrary stand-in for the 4-byte PLONK verifier selector
    /// (`plonk_vkey_hash[..4]`) that prefixes the on-chain seal.
    const VKEY_PREFIX: [u8; 4] = [0x5a, 0x09, 0x3a, 0x2f];

    #[test]
    fn encode_onchain_zk_proof_prefixes_zk_type_and_vkey() {
        let receipt_bytes = SnarkReceiptFixture::plonk_receipt_bytes(VKEY_PREFIX, "abcd");
        let proof = SnarkReceiptEncoder::encode_onchain_zk_proof(&receipt_bytes)
            .expect("valid receipt encodes");

        assert_eq!(proof.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
    }

    #[test]
    fn encode_onchain_zk_proof_rejects_invalid_receipts() {
        let error = SnarkReceiptEncoder::encode_onchain_zk_proof(b"not-an-sp1-receipt")
            .expect_err("invalid receipt must fail");

        assert!(matches!(error, SnarkReceiptDecodeError::Receipt(_)));
    }

    #[test]
    fn encode_onchain_zk_proof_rejects_non_plonk_receipts() {
        let receipt = SP1ProofWithPublicValues {
            proof: SP1Proof::Groth16(Default::default()),
            public_values: SP1PublicValues::new(),
            sp1_version: "test".to_owned(),
            tee_proof: None,
        };

        let error = SnarkReceiptEncoder::encode_onchain_zk_proof(
            &SnarkReceiptFixture::receipt_bytes(&receipt),
        )
        .expect_err("non-PLONK receipt must fail");

        assert!(matches!(error, SnarkReceiptDecodeError::NotPlonk));
    }

    #[test]
    fn encode_onchain_zk_proof_rejects_tee_2fa_receipts() {
        let mut receipt = SnarkReceiptFixture::plonk_receipt(VKEY_PREFIX, "abcd");
        receipt.tee_proof = Some(vec![0xEE; 8]);

        let error = SnarkReceiptEncoder::encode_onchain_zk_proof(
            &SnarkReceiptFixture::receipt_bytes(&receipt),
        )
        .expect_err("TEE-2FA receipt must fail");

        assert!(matches!(error, SnarkReceiptDecodeError::TeeProof));
    }

    #[test]
    fn encode_onchain_zk_proof_rejects_mock_proofs() {
        let receipt_bytes = SnarkReceiptFixture::plonk_receipt_bytes(VKEY_PREFIX, "");
        let error = SnarkReceiptEncoder::encode_onchain_zk_proof(&receipt_bytes)
            .expect_err("mock receipt must fail");

        assert!(matches!(error, SnarkReceiptDecodeError::MockProof));
    }

    #[test]
    fn encode_onchain_zk_proof_rejects_malformed_proof_hex() {
        let receipt_bytes = SnarkReceiptFixture::plonk_receipt_bytes(VKEY_PREFIX, "zzzz");
        let error = SnarkReceiptEncoder::encode_onchain_zk_proof(&receipt_bytes)
            .expect_err("malformed proof hex must fail");

        assert!(matches!(error, SnarkReceiptDecodeError::ProofHex(_)));
    }

    #[test]
    fn encode_onchain_zk_proof_skips_populated_receipt_fields() {
        let mut receipt = SnarkReceiptFixture::plonk_receipt(VKEY_PREFIX, "abcd");
        receipt.public_values = SP1PublicValues::from(&[0x11; 96][..]);
        receipt.sp1_version = "v6.10.0".to_owned();
        let SP1Proof::Plonk(plonk) = &mut receipt.proof else {
            unreachable!("fixture is a PLONK receipt");
        };
        plonk.public_inputs = std::array::from_fn(|i| format!("{}", u128::MAX - i as u128));
        plonk.raw_proof = "ef".repeat(400);
        plonk.plonk_vkey_hash[4..].fill(0xCC);

        let proof = SnarkReceiptEncoder::encode_onchain_zk_proof(
            &SnarkReceiptFixture::receipt_bytes(&receipt),
        )
        .expect("populated receipt encodes");

        assert_eq!(proof.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
    }
}
