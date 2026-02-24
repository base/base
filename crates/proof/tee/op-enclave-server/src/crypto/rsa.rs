//! RSA cryptographic operations.
//!
//! This module provides RSA-4096 key generation, PKIX serialization,
//! and `PKCS1v15` encryption/decryption.

use rand::CryptoRng;
use rsa::pkcs1v15::{DecryptingKey, EncryptingKey};
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::traits::{Decryptor, RandomizedEncryptor};
use rsa::{RsaPrivateKey, RsaPublicKey};

use crate::error::{CryptoError, ServerError};

/// RSA key size in bits.
pub const RSA_KEY_BITS: usize = 4096;

/// Generate a new RSA-4096 private key.
pub fn generate_rsa_key<R: CryptoRng + rand::RngCore>(
    rng: &mut R,
) -> Result<RsaPrivateKey, ServerError> {
    RsaPrivateKey::new(rng, RSA_KEY_BITS)
        .map_err(|e| CryptoError::RsaKeyGeneration(e.to_string()).into())
}

/// Serialize an RSA public key to PKIX/DER format.
///
/// This matches Go's `x509.MarshalPKIXPublicKey()`.
pub fn public_key_to_pkix(key: &RsaPublicKey) -> Result<Vec<u8>, ServerError> {
    key.to_public_key_der()
        .map(|doc| doc.as_bytes().to_vec())
        .map_err(|e| CryptoError::PkixSerialize(e.to_string()).into())
}

/// Parse an RSA public key from PKIX/DER format.
///
/// This matches Go's `x509.ParsePKIXPublicKey()`.
pub fn pkix_to_public_key(bytes: &[u8]) -> Result<RsaPublicKey, ServerError> {
    RsaPublicKey::from_public_key_der(bytes)
        .map_err(|e| CryptoError::PkixParse(e.to_string()).into())
}

/// Encrypt data using RSA PKCS#1 v1.5.
pub fn encrypt_pkcs1v15<R: CryptoRng + rand::RngCore>(
    rng: &mut R,
    public_key: &RsaPublicKey,
    data: &[u8],
) -> Result<Vec<u8>, ServerError> {
    let encrypting_key = EncryptingKey::new(public_key.clone());
    encrypting_key
        .encrypt_with_rng(rng, data)
        .map_err(|e| CryptoError::RsaEncrypt(e.to_string()).into())
}

/// Decrypt data using RSA PKCS#1 v1.5.
pub fn decrypt_pkcs1v15(
    private_key: &RsaPrivateKey,
    ciphertext: &[u8],
) -> Result<Vec<u8>, ServerError> {
    let decrypting_key = DecryptingKey::new(private_key.clone());
    decrypting_key
        .decrypt(ciphertext)
        .map_err(|e| CryptoError::RsaDecrypt(e.to_string()).into())
}

/// Get the public key from a private key.
pub fn private_to_public(private_key: &RsaPrivateKey) -> RsaPublicKey {
    private_key.to_public_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_generate_rsa_key() {
        use rsa::traits::PublicKeyParts;
        let mut rng = OsRng;
        let key = generate_rsa_key(&mut rng).expect("failed to generate key");
        assert_eq!(key.size() * 8, RSA_KEY_BITS);
    }

    #[test]
    fn test_pkix_roundtrip() {
        let mut rng = OsRng;
        let private_key = generate_rsa_key(&mut rng).expect("failed to generate key");
        let public_key = private_to_public(&private_key);

        let pkix = public_key_to_pkix(&public_key).expect("failed to serialize");
        let parsed = pkix_to_public_key(&pkix).expect("failed to parse");

        assert_eq!(public_key, parsed);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let mut rng = OsRng;
        let private_key = generate_rsa_key(&mut rng).expect("failed to generate key");
        let public_key = private_to_public(&private_key);

        let plaintext = b"Hello, World!";
        let ciphertext =
            encrypt_pkcs1v15(&mut rng, &public_key, plaintext).expect("failed to encrypt");
        let decrypted = decrypt_pkcs1v15(&private_key, &ciphertext).expect("failed to decrypt");

        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_encrypt_32_byte_key() {
        let mut rng = OsRng;
        let private_key = generate_rsa_key(&mut rng).expect("failed to generate key");
        let public_key = private_to_public(&private_key);

        // Simulate encrypting a 32-byte ECDSA private key
        let ecdsa_key = [0x42u8; 32];
        let ciphertext =
            encrypt_pkcs1v15(&mut rng, &public_key, &ecdsa_key).expect("failed to encrypt");
        let decrypted = decrypt_pkcs1v15(&private_key, &ciphertext).expect("failed to decrypt");

        assert_eq!(ecdsa_key.as_slice(), decrypted.as_slice());
    }
}
