//! ECDSA secp256k1 key generation, signing, and verification.

use alloy_primitives::{Address, Bytes, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_proof_primitives::ECDSA_SIGNATURE_LENGTH;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::hazmat::PrehashVerifier};
use rand_08::CryptoRng;

use crate::error::{CryptoError, ProposalError, Result};

/// ECDSA secp256k1 operations.
#[derive(Debug)]
pub struct Ecdsa;

impl Ecdsa {
    /// Generate a new signer with a random private key.
    pub fn generate<R: CryptoRng + rand_08::RngCore>(rng: &mut R) -> Result<PrivateKeySigner> {
        let signing_key = SigningKey::random(rng);
        Ok(PrivateKeySigner::from_signing_key(signing_key))
    }

    /// Create a signer from a 32-byte private key.
    pub fn from_bytes(bytes: &[u8]) -> Result<PrivateKeySigner> {
        let signing_key =
            SigningKey::from_slice(bytes).map_err(|e| CryptoError::EcdsaKeyParse(e.to_string()))?;
        Ok(PrivateKeySigner::from_signing_key(signing_key))
    }

    /// Create a signer from a hex-encoded private key.
    ///
    /// The hex string may optionally be prefixed with "0x".
    pub fn from_hex(hex_str: &str) -> Result<PrivateKeySigner> {
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        let bytes = alloy_primitives::hex::decode(hex_str)
            .map_err(|e| CryptoError::HexParse(e.to_string()))?;
        Self::from_bytes(&bytes)
    }

    /// Get the uncompressed 65-byte public key (0x04 || x || y).
    ///
    /// Matches Go's `crypto.FromECDSAPub()` format.
    pub fn public_key_bytes(signer: &PrivateKeySigner) -> Vec<u8> {
        let verifying_key = signer.credential().verifying_key();
        let encoded_point = verifying_key.to_encoded_point(false);
        encoded_point.as_bytes().to_vec()
    }

    /// Get the Ethereum address from a signer.
    pub const fn address(signer: &PrivateKeySigner) -> Address {
        signer.address()
    }
}

/// Proposal signing and verification.
#[derive(Debug)]
pub struct Signing;

impl Signing {
    /// Sign data with keccak256 hash, returns 65-byte signature (r || s || v).
    pub fn sign(signer: &PrivateKeySigner, data: &[u8]) -> Result<Bytes> {
        let hash = keccak256(data);
        let signature = signer
            .sign_hash_sync(&hash)
            .map_err(|e| ProposalError::SigningFailed(e.to_string()))?;

        let sig_bytes = signature.as_rsy();
        Ok(Bytes::from(sig_bytes.to_vec()))
    }

    /// Verify a proposal signature.
    ///
    /// Uses only the first 64 bytes (r, s), matching Go's `crypto.VerifySignature`.
    pub fn verify(public_key: &[u8], data: &[u8], signature: &[u8]) -> Result<bool> {
        if signature.len() != ECDSA_SIGNATURE_LENGTH {
            return Err(ProposalError::InvalidSignatureLength(signature.len()).into());
        }

        let verifying_key = VerifyingKey::from_sec1_bytes(public_key)
            .map_err(|e| ProposalError::RecoveryFailed(e.to_string()))?;

        let sig = Signature::from_slice(&signature[..64])
            .map_err(|e| ProposalError::SigningFailed(format!("failed to parse signature: {e}")))?;

        let hash = keccak256(data);
        Ok(verifying_key.verify_prehash(hash.as_slice(), &sig).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use base_proof_primitives::{PROOF_JOURNAL_BASE_LENGTH, ProofJournal};
    use rand_08::rngs::OsRng;

    use super::*;
    use crate::NitroError;

    fn test_journal() -> ProofJournal {
        ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: b256!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
            prev_output_root: b256!(
                "3333333333333333333333333333333333333333333333333333333333333333"
            ),
            starting_l2_block: 999,
            output_root: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            ending_l2_block: 1000,
            intermediate_roots: vec![],
            config_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            tee_image_hash: b256!(
                "5555555555555555555555555555555555555555555555555555555555555555"
            ),
        }
    }

    #[test]
    fn test_generate_signer() {
        let mut rng = OsRng;
        let signer = Ecdsa::generate(&mut rng).expect("failed to generate signer");
        let public_key = Ecdsa::public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    fn test_signer_from_hex() {
        let hex_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = Ecdsa::from_hex(hex_key).expect("failed to parse hex key");
        let public_key = Ecdsa::public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
    }

    #[test]
    fn test_signer_from_hex_no_prefix() {
        let hex_key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = Ecdsa::from_hex(hex_key).expect("failed to parse hex key");
        let public_key = Ecdsa::public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
    }

    #[test]
    fn test_invalid_private_key() {
        let short_key = [0u8; 31];
        let result = Ecdsa::from_bytes(&short_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_signer_address() {
        let hex_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = Ecdsa::from_hex(hex_key).expect("failed to parse hex key");
        let address = Ecdsa::address(&signer);
        let expected = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse::<Address>().unwrap();
        assert_eq!(address, expected);
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let mut rng = OsRng;
        let signer = Ecdsa::generate(&mut rng).expect("failed to generate signer");

        let data = test_journal().encode();
        let signature = Signing::sign(&signer, &data).expect("signing failed");
        assert_eq!(signature.len(), ECDSA_SIGNATURE_LENGTH);

        let public_key = Ecdsa::public_key_bytes(&signer);
        let valid = Signing::verify(&public_key, &data, &signature).expect("verification failed");
        assert!(valid);
    }

    #[test]
    fn test_verify_wrong_key_fails() {
        let mut rng = OsRng;
        let signer1 = Ecdsa::generate(&mut rng).expect("failed to generate signer");
        let signer2 = Ecdsa::generate(&mut rng).expect("failed to generate signer");

        let data = test_journal().encode();
        let signature = Signing::sign(&signer1, &data).expect("signing failed");

        let wrong_public_key = Ecdsa::public_key_bytes(&signer2);
        let valid =
            Signing::verify(&wrong_public_key, &data, &signature).expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_verify_wrong_data_fails() {
        let mut rng = OsRng;
        let signer = Ecdsa::generate(&mut rng).expect("failed to generate signer");

        let data1 = test_journal().encode();

        let journal2 = ProofJournal {
            proposer: address!("0000000000000000000000000000000000000001"),
            ..test_journal()
        };
        let data2 = journal2.encode();

        let signature = Signing::sign(&signer, &data1).expect("signing failed");

        let public_key = Ecdsa::public_key_bytes(&signer);
        let valid = Signing::verify(&public_key, &data2, &signature).expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_invalid_signature_length() {
        let public_key = vec![0x04; 65];
        let data = [0u8; PROOF_JOURNAL_BASE_LENGTH];
        let short_sig = vec![0u8; 64];

        let result = Signing::verify(&public_key, &data, &short_sig);
        assert!(matches!(
            result,
            Err(NitroError::Proposal(ProposalError::InvalidSignatureLength(64)))
        ));
    }
}
