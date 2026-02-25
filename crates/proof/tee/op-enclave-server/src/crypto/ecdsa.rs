//! ECDSA secp256k1 operations.
//!
//! This module provides ECDSA key generation and management using
//! `alloy-signer-local::PrivateKeySigner`.

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use k256::ecdsa::SigningKey;
use rand_08::CryptoRng;

use crate::error::{CryptoError, ServerError};

/// Generate a new ECDSA signer with a random private key.
pub fn generate_signer<R: CryptoRng + rand_08::RngCore>(
    rng: &mut R,
) -> Result<PrivateKeySigner, ServerError> {
    let signing_key = SigningKey::random(rng);
    Ok(PrivateKeySigner::from_signing_key(signing_key))
}

/// Create a signer from a 32-byte private key.
pub fn signer_from_bytes(bytes: &[u8]) -> Result<PrivateKeySigner, ServerError> {
    if bytes.len() != 32 {
        return Err(CryptoError::InvalidPrivateKeyLength(bytes.len()).into());
    }

    let signing_key =
        SigningKey::from_slice(bytes).map_err(|e| CryptoError::EcdsaKeyParse(e.to_string()))?;
    Ok(PrivateKeySigner::from_signing_key(signing_key))
}

/// Create a signer from a hex-encoded private key.
///
/// The hex string may optionally be prefixed with "0x".
pub fn signer_from_hex(hex_str: &str) -> Result<PrivateKeySigner, ServerError> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(hex_str).map_err(|e| CryptoError::HexParse(e.to_string()))?;
    signer_from_bytes(&bytes)
}

/// Get the uncompressed 65-byte public key.
///
/// This matches Go's `crypto.FromECDSAPub()` format:
/// - 1 byte: 0x04 (uncompressed point marker)
/// - 32 bytes: X coordinate
/// - 32 bytes: Y coordinate
pub fn public_key_bytes(signer: &PrivateKeySigner) -> Vec<u8> {
    let verifying_key = signer.credential().verifying_key();
    let encoded_point = verifying_key.to_encoded_point(false);
    encoded_point.as_bytes().to_vec()
}

/// Get the 32-byte private key.
///
/// This matches Go's `crypto.FromECDSA()` format.
pub fn private_key_bytes(signer: &PrivateKeySigner) -> Vec<u8> {
    signer.credential().to_bytes().to_vec()
}

/// Get the Ethereum address from a signer.
pub const fn signer_address(signer: &PrivateKeySigner) -> Address {
    signer.address()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use rand_08::rngs::OsRng;

    use super::{
        generate_signer, private_key_bytes, public_key_bytes, signer_address, signer_from_bytes,
        signer_from_hex,
    };

    #[test]
    fn test_generate_signer() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");
        let public_key = public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04); // Uncompressed point marker
    }

    #[test]
    fn test_private_key_length() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");
        let private_key = private_key_bytes(&signer);
        assert_eq!(private_key.len(), 32);
    }

    #[test]
    fn test_signer_from_bytes_roundtrip() {
        let mut rng = OsRng;
        let signer1 = generate_signer(&mut rng).expect("failed to generate signer");
        let private_key = private_key_bytes(&signer1);

        let signer2 = signer_from_bytes(&private_key).expect("failed to parse signer");
        assert_eq!(private_key_bytes(&signer1), private_key_bytes(&signer2));
        assert_eq!(public_key_bytes(&signer1), public_key_bytes(&signer2));
    }

    #[test]
    fn test_signer_from_hex() {
        // Test with a known private key
        let hex_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = signer_from_hex(hex_key).expect("failed to parse hex key");
        let public_key = public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
    }

    #[test]
    fn test_signer_from_hex_no_prefix() {
        let hex_key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = signer_from_hex(hex_key).expect("failed to parse hex key");
        let public_key = public_key_bytes(&signer);
        assert_eq!(public_key.len(), 65);
    }

    #[test]
    fn test_invalid_private_key_length() {
        let short_key = [0u8; 31];
        let result = signer_from_bytes(&short_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_signer_address() {
        // Test with a known private key (Anvil's first account)
        let hex_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = signer_from_hex(hex_key).expect("failed to parse hex key");
        let address = signer_address(&signer);
        // Anvil's first account address
        let expected = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse::<Address>().unwrap();
        assert_eq!(address, expected);
    }
}
