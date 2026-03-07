//! RSA cryptographic operations.
//!
//! This module provides RSA-4096 key generation, PKIX serialization,
//! and `PKCS1v15` encryption/decryption.

use rand_08::CryptoRng;
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1v15::{DecryptingKey, EncryptingKey},
    pkcs8::{DecodePublicKey, EncodePublicKey},
    traits::{Decryptor, RandomizedEncryptor},
};

/// RSA key size in bits.
pub const RSA_KEY_BITS: usize = 4096;

/// Generate a new RSA-4096 private key.
pub fn generate_rsa_key<R: CryptoRng + rand_08::RngCore>(
    rng: &mut R,
) -> eyre::Result<RsaPrivateKey> {
    Ok(RsaPrivateKey::new(rng, RSA_KEY_BITS)?)
}

/// Serialize an RSA public key to PKIX/DER format.
///
/// This matches Go's `x509.MarshalPKIXPublicKey()`.
pub fn public_key_to_pkix(key: &RsaPublicKey) -> eyre::Result<Vec<u8>> {
    Ok(key.to_public_key_der().map(|doc| doc.as_bytes().to_vec())?)
}

/// Parse an RSA public key from PKIX/DER format.
///
/// This matches Go's `x509.ParsePKIXPublicKey()`.
pub fn pkix_to_public_key(bytes: &[u8]) -> eyre::Result<RsaPublicKey> {
    Ok(RsaPublicKey::from_public_key_der(bytes)?)
}

/// Encrypt data using RSA PKCS#1 v1.5.
pub fn encrypt_pkcs1v15<R: CryptoRng + rand_08::RngCore>(
    rng: &mut R,
    public_key: &RsaPublicKey,
    data: &[u8],
) -> eyre::Result<Vec<u8>> {
    let encrypting_key = EncryptingKey::new(public_key.clone());
    Ok(encrypting_key.encrypt_with_rng(rng, data)?)
}

/// Decrypt data using RSA PKCS#1 v1.5.
pub fn decrypt_pkcs1v15(private_key: &RsaPrivateKey, ciphertext: &[u8]) -> eyre::Result<Vec<u8>> {
    let decrypting_key = DecryptingKey::new(private_key.clone());
    Ok(decrypting_key.decrypt(ciphertext)?)
}

/// Get the public key from a private key.
pub fn private_to_public(private_key: &RsaPrivateKey) -> RsaPublicKey {
    private_key.to_public_key()
}

#[cfg(test)]
mod tests {
    use rand_08::rngs::OsRng;
    use rsa::traits::PublicKeyParts;

    use super::*;

    #[test]
    fn test_generate_rsa_key() {
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
