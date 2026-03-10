/// ECDSA secp256k1 operations.
///
/// Provides key generation, parsing, and address derivation using
/// `alloy-signer-local::PrivateKeySigner`.
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use k256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::hazmat::PrehashVerifier};
use rand_08::CryptoRng;

use crate::error::{CryptoError, ProposalError, Result};

/// Expected length of an ECDSA signature (r: 32 bytes, s: 32 bytes, v: 1 byte).
pub const SIGNATURE_LENGTH: usize = 65;

/// Base length of the signing data without intermediate roots:
/// address(20) + 7 x bytes32(32) = 244 bytes.
pub const SIGNING_DATA_BASE_LENGTH: usize = 244;

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

/// Proposal signing and verification (`AggregateVerifier` journal format).
#[derive(Debug)]
pub struct Signing;

impl Signing {
    /// Build the signing data for a proposal.
    ///
    /// The format matches the `AggregateVerifier` contract's journal:
    /// ```text
    /// prover(20) || l1OriginHash(32) || prevOutputRoot(32)
    ///   || startingL2Block(32) || outputRoot(32) || endingL2Block(32)
    ///   || intermediateRoots(32*N) || configHash(32) || imageHash(32)
    /// ```
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn build_data(
        proposer: Address,
        l1_origin_hash: B256,
        prev_output_root: B256,
        starting_l2_block: U256,
        output_root: B256,
        ending_l2_block: U256,
        intermediate_roots: &[B256],
        config_hash: B256,
        tee_image_hash: B256,
    ) -> Vec<u8> {
        let len = SIGNING_DATA_BASE_LENGTH + 32 * intermediate_roots.len();
        let mut data = vec![0u8; len];
        let mut offset = 0;

        data[offset..offset + 20].copy_from_slice(proposer.as_slice());
        offset += 20;

        data[offset..offset + 32].copy_from_slice(l1_origin_hash.as_slice());
        offset += 32;

        data[offset..offset + 32].copy_from_slice(prev_output_root.as_slice());
        offset += 32;

        data[offset..offset + 32].copy_from_slice(&starting_l2_block.to_be_bytes::<32>());
        offset += 32;

        data[offset..offset + 32].copy_from_slice(output_root.as_slice());
        offset += 32;

        data[offset..offset + 32].copy_from_slice(&ending_l2_block.to_be_bytes::<32>());
        offset += 32;

        for root in intermediate_roots {
            data[offset..offset + 32].copy_from_slice(root.as_slice());
            offset += 32;
        }

        data[offset..offset + 32].copy_from_slice(config_hash.as_slice());
        offset += 32;

        data[offset..offset + 32].copy_from_slice(tee_image_hash.as_slice());

        data
    }

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
        if signature.len() != SIGNATURE_LENGTH {
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
    use rand_08::rngs::OsRng;

    use super::*;
    use crate::NitroError;

    fn test_signing_data() -> Vec<u8> {
        Signing::build_data(
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            U256::from(999),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            U256::from(1000),
            &[],
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("5555555555555555555555555555555555555555555555555555555555555555"),
        )
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
    fn test_build_signing_data_length() {
        let data = test_signing_data();
        assert_eq!(data.len(), SIGNING_DATA_BASE_LENGTH);
        assert_eq!(data.len(), 244);
    }

    #[test]
    fn test_build_signing_data_components() {
        let proposer = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let l1_origin_hash =
            b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let prev_output_root =
            b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let starting_l2_block = U256::from(999);
        let output_root = b256!("4444444444444444444444444444444444444444444444444444444444444444");
        let ending_l2_block = U256::from(1000);
        let config_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let tee_image_hash =
            b256!("5555555555555555555555555555555555555555555555555555555555555555");

        let data = Signing::build_data(
            proposer,
            l1_origin_hash,
            prev_output_root,
            starting_l2_block,
            output_root,
            ending_l2_block,
            &[],
            config_hash,
            tee_image_hash,
        );

        let mut off = 0;
        assert_eq!(&data[off..off + 20], proposer.as_slice());
        off += 20;
        assert_eq!(&data[off..off + 32], l1_origin_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], prev_output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &starting_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &ending_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], config_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], tee_image_hash.as_slice());
    }

    #[test]
    fn test_build_signing_data_with_intermediate_roots() {
        let proposer = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let intermediate_roots = vec![
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ];

        let data = Signing::build_data(
            proposer,
            B256::ZERO,
            B256::ZERO,
            U256::ZERO,
            B256::ZERO,
            U256::ZERO,
            &intermediate_roots,
            B256::ZERO,
            B256::ZERO,
        );

        assert_eq!(data.len(), SIGNING_DATA_BASE_LENGTH + 64);

        let ir_offset = 20 + 5 * 32;
        assert_eq!(&data[ir_offset..ir_offset + 32], intermediate_roots[0].as_slice());
        assert_eq!(&data[ir_offset + 32..ir_offset + 64], intermediate_roots[1].as_slice());
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let mut rng = OsRng;
        let signer = Ecdsa::generate(&mut rng).expect("failed to generate signer");

        let data = test_signing_data();
        let signature = Signing::sign(&signer, &data).expect("signing failed");
        assert_eq!(signature.len(), SIGNATURE_LENGTH);

        let public_key = Ecdsa::public_key_bytes(&signer);
        let valid = Signing::verify(&public_key, &data, &signature).expect("verification failed");
        assert!(valid);
    }

    #[test]
    fn test_verify_wrong_key_fails() {
        let mut rng = OsRng;
        let signer1 = Ecdsa::generate(&mut rng).expect("failed to generate signer");
        let signer2 = Ecdsa::generate(&mut rng).expect("failed to generate signer");

        let data = test_signing_data();
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

        let data1 = test_signing_data();

        let data2 = Signing::build_data(
            address!("0000000000000000000000000000000000000001"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            U256::from(999),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            U256::from(1000),
            &[],
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("5555555555555555555555555555555555555555555555555555555555555555"),
        );

        let signature = Signing::sign(&signer, &data1).expect("signing failed");

        let public_key = Ecdsa::public_key_bytes(&signer);
        let valid = Signing::verify(&public_key, &data2, &signature).expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_invalid_signature_length() {
        let public_key = vec![0x04; 65];
        let data = [0u8; SIGNING_DATA_BASE_LENGTH];
        let short_sig = vec![0u8; 64];

        let result = Signing::verify(&public_key, &data, &short_sig);
        assert!(matches!(
            result,
            Err(NitroError::Proposal(ProposalError::InvalidSignatureLength(64)))
        ));
    }
}
