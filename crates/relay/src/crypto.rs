//! ECIES encryption for encrypted transaction relay.
//!
//! Uses X25519 for key exchange and ChaCha20-Poly1305 for authenticated encryption.

use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng},
};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

use crate::error::RelayError;

/// Size of X25519 public key in bytes.
pub const PUBLIC_KEY_SIZE: usize = 32;

/// Size of ChaCha20-Poly1305 nonce in bytes.
pub const NONCE_SIZE: usize = 12;

/// Size of ChaCha20-Poly1305 authentication tag in bytes.
pub const TAG_SIZE: usize = 16;

/// HKDF info string for deriving the symmetric key.
const HKDF_INFO: &[u8] = b"base-encrypted-relay-v1";

/// Encrypts a message using ECIES.
///
/// # Arguments
/// * `recipient_pubkey` - The recipient's X25519 public key (32 bytes)
/// * `plaintext` - The message to encrypt
///
/// # Returns
/// Ciphertext in format: `ephemeral_pubkey (32) || nonce (12) || ciphertext || tag (16)`
pub fn encrypt(recipient_pubkey: &[u8; PUBLIC_KEY_SIZE], plaintext: &[u8]) -> Result<Vec<u8>, RelayError> {
    // Generate ephemeral keypair
    let ephemeral_secret = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);

    // Perform X25519 key exchange
    let recipient_public = PublicKey::from(*recipient_pubkey);
    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_public);

    // Derive symmetric key using HKDF
    let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut symmetric_key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut symmetric_key)
        .map_err(|_| RelayError::CryptoError("HKDF expansion failed".to_string()))?;

    // Encrypt with ChaCha20-Poly1305
    let cipher = ChaCha20Poly1305::new_from_slice(&symmetric_key)
        .map_err(|e| RelayError::CryptoError(format!("cipher init failed: {e}")))?;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| RelayError::CryptoError(format!("encryption failed: {e}")))?;

    // Assemble output: ephemeral_pubkey || nonce || ciphertext (includes tag)
    let mut output = Vec::with_capacity(PUBLIC_KEY_SIZE + NONCE_SIZE + ciphertext.len());
    output.extend_from_slice(ephemeral_public.as_bytes());
    output.extend_from_slice(nonce.as_slice());
    output.extend_from_slice(&ciphertext);

    Ok(output)
}

/// Decrypts an ECIES ciphertext.
///
/// # Arguments
/// * `secret_key` - The recipient's X25519 secret key (32 bytes)
/// * `ciphertext` - The encrypted message in format: `ephemeral_pubkey || nonce || ciphertext || tag`
///
/// # Returns
/// The decrypted plaintext.
pub fn decrypt(secret_key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, RelayError> {
    // Minimum size: ephemeral_pubkey + nonce + tag (empty plaintext produces just tag)
    let min_size = PUBLIC_KEY_SIZE + NONCE_SIZE + TAG_SIZE;
    if ciphertext.len() < min_size {
        return Err(RelayError::CryptoError(format!(
            "ciphertext too short: {} < {min_size}",
            ciphertext.len()
        )));
    }

    // Parse components
    let ephemeral_pubkey: [u8; PUBLIC_KEY_SIZE] = ciphertext[..PUBLIC_KEY_SIZE]
        .try_into()
        .map_err(|_| RelayError::CryptoError("invalid ephemeral pubkey".to_string()))?;
    let nonce_bytes: [u8; NONCE_SIZE] = ciphertext[PUBLIC_KEY_SIZE..PUBLIC_KEY_SIZE + NONCE_SIZE]
        .try_into()
        .map_err(|_| RelayError::CryptoError("invalid nonce".to_string()))?;
    let encrypted_data = &ciphertext[PUBLIC_KEY_SIZE + NONCE_SIZE..];

    // Perform X25519 key exchange
    let static_secret = StaticSecret::from(*secret_key);
    let ephemeral_public = PublicKey::from(ephemeral_pubkey);
    let shared_secret = static_secret.diffie_hellman(&ephemeral_public);

    // Derive symmetric key using HKDF
    let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut symmetric_key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut symmetric_key)
        .map_err(|_| RelayError::CryptoError("HKDF expansion failed".to_string()))?;

    // Decrypt with ChaCha20-Poly1305
    let cipher = ChaCha20Poly1305::new_from_slice(&symmetric_key)
        .map_err(|e| RelayError::CryptoError(format!("cipher init failed: {e}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, encrypted_data)
        .map_err(|e| RelayError::CryptoError(format!("decryption failed: {e}")))?;

    Ok(plaintext)
}

/// Generates a new X25519 keypair.
///
/// # Returns
/// A tuple of (secret_key, public_key), both as 32-byte arrays.
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let (secret_key, public_key) = generate_keypair();
        let plaintext = b"Hello, encrypted mempool!";

        let ciphertext = encrypt(&public_key, plaintext).expect("encryption should succeed");

        // Verify ciphertext structure
        assert!(ciphertext.len() >= PUBLIC_KEY_SIZE + NONCE_SIZE + TAG_SIZE + plaintext.len());

        let decrypted = decrypt(&secret_key, &ciphertext).expect("decryption should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts() {
        let (_, public_key) = generate_keypair();
        let plaintext = b"test message";

        let ct1 = encrypt(&public_key, plaintext).unwrap();
        let ct2 = encrypt(&public_key, plaintext).unwrap();

        // Different ephemeral keys should produce different ciphertexts
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn test_decrypt_wrong_key_fails() {
        let (_, public_key) = generate_keypair();
        let (wrong_secret, _) = generate_keypair();
        let plaintext = b"secret message";

        let ciphertext = encrypt(&public_key, plaintext).unwrap();
        let result = decrypt(&wrong_secret, &ciphertext);

        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_tampered_ciphertext_fails() {
        let (secret_key, public_key) = generate_keypair();
        let plaintext = b"important data";

        let mut ciphertext = encrypt(&public_key, plaintext).unwrap();

        // Tamper with the ciphertext
        let last_idx = ciphertext.len() - 1;
        ciphertext[last_idx] ^= 0xFF;

        let result = decrypt(&secret_key, &ciphertext);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_too_short_fails() {
        let (secret_key, _) = generate_keypair();

        // Too short to contain all components
        let short_ciphertext = vec![0u8; PUBLIC_KEY_SIZE + NONCE_SIZE + TAG_SIZE];
        let result = decrypt(&secret_key, &short_ciphertext);

        assert!(result.is_err());
    }

    #[test]
    fn test_empty_plaintext() {
        let (secret_key, public_key) = generate_keypair();
        let plaintext = b"";

        let ciphertext = encrypt(&public_key, plaintext).unwrap();
        let decrypted = decrypt(&secret_key, &ciphertext).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_large_plaintext() {
        let (secret_key, public_key) = generate_keypair();
        let plaintext = vec![0xABu8; 100_000]; // 100KB

        let ciphertext = encrypt(&public_key, &plaintext).unwrap();
        let decrypted = decrypt(&secret_key, &ciphertext).unwrap();

        assert_eq!(decrypted, plaintext);
    }
}
