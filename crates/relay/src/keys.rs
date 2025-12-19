//! Ed25519/X25519 key management for the encrypted relay.
//!
//! The sequencer uses a single Ed25519 keypair for both:
//! - **Attestation signing**: Ed25519 signatures prove sequencer identity
//! - **Transaction decryption**: X25519 derived from Ed25519 for ECIES decryption
//!
//! This module provides utilities for:
//! - Generating Ed25519 keypairs
//! - Deriving X25519 keys from Ed25519 keys
//! - Loading/saving keypairs to files

use std::path::Path;

use alloy_primitives::hex;
use ed25519_dalek::SigningKey as Ed25519SigningKey;
use sha2::{Digest, Sha512};
use x25519_dalek::StaticSecret as X25519StaticSecret;

use crate::error::RelayError;

/// A sequencer keypair containing both Ed25519 (for signing) and derived X25519 (for decryption).
#[derive(Clone)]
pub struct SequencerKeypair {
    /// Ed25519 signing key for attestations.
    ed25519_signing_key: Ed25519SigningKey,
    /// X25519 static secret derived from Ed25519, for ECIES decryption.
    x25519_secret: X25519StaticSecret,
}

impl std::fmt::Debug for SequencerKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequencerKeypair")
            .field("ed25519_pubkey", &hex::encode(self.ed25519_signing_key.verifying_key().as_bytes()))
            .field("x25519_pubkey", &hex::encode(self.x25519_public_key()))
            .finish()
    }
}

impl SequencerKeypair {
    /// Creates a new keypair from an Ed25519 signing key.
    ///
    /// The X25519 secret is derived from the Ed25519 seed using the standard
    /// conversion (same as libsodium's `crypto_sign_ed25519_sk_to_curve25519`).
    pub fn from_ed25519(ed25519_signing_key: Ed25519SigningKey) -> Self {
        let x25519_secret = derive_x25519_from_ed25519(&ed25519_signing_key);
        Self {
            ed25519_signing_key,
            x25519_secret,
        }
    }

    /// Generates a new random keypair.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let ed25519_signing_key = Ed25519SigningKey::generate(&mut rng);
        Self::from_ed25519(ed25519_signing_key)
    }

    /// Loads a keypair from a file containing the 32-byte Ed25519 seed.
    ///
    /// The file can contain either:
    /// - Raw 32 bytes (binary)
    /// - 64 hex characters (with optional 0x prefix)
    pub fn load_from_file(path: &Path) -> Result<Self, RelayError> {
        let contents = std::fs::read(path)
            .map_err(|e| RelayError::ConfigError(format!("failed to read keypair file: {e}")))?;

        let seed = parse_key_bytes(&contents)?;
        let ed25519_signing_key = Ed25519SigningKey::from_bytes(&seed);
        Ok(Self::from_ed25519(ed25519_signing_key))
    }

    /// Creates a keypair from a hex-encoded Ed25519 seed.
    ///
    /// Accepts 64 hex characters with optional 0x prefix.
    /// This is useful for passing keys via CLI or environment variables.
    pub fn from_hex(hex_str: &str) -> Result<Self, RelayError> {
        let seed = parse_key_bytes(hex_str.as_bytes())?;
        let ed25519_signing_key = Ed25519SigningKey::from_bytes(&seed);
        Ok(Self::from_ed25519(ed25519_signing_key))
    }

    /// Saves the keypair seed to a file (32 bytes, hex-encoded).
    pub fn save_to_file(&self, path: &Path) -> Result<(), RelayError> {
        let hex_seed = hex::encode(self.ed25519_signing_key.to_bytes());
        std::fs::write(path, hex_seed)
            .map_err(|e| RelayError::ConfigError(format!("failed to write keypair file: {e}")))
    }

    /// Returns a reference to the Ed25519 signing key.
    pub fn ed25519_signing_key(&self) -> &Ed25519SigningKey {
        &self.ed25519_signing_key
    }

    /// Returns the Ed25519 public key bytes (32 bytes).
    pub fn ed25519_public_key(&self) -> [u8; 32] {
        self.ed25519_signing_key.verifying_key().to_bytes()
    }

    /// Returns a reference to the X25519 static secret.
    pub fn x25519_secret(&self) -> &X25519StaticSecret {
        &self.x25519_secret
    }

    /// Returns the X25519 secret key bytes (32 bytes).
    ///
    /// This is what gets passed to the `decrypt` function.
    pub fn x25519_secret_bytes(&self) -> [u8; 32] {
        self.x25519_secret.to_bytes()
    }

    /// Returns the X25519 public key bytes (32 bytes).
    ///
    /// This is the encryption public key that clients use.
    pub fn x25519_public_key(&self) -> [u8; 32] {
        let public = x25519_dalek::PublicKey::from(&self.x25519_secret);
        public.to_bytes()
    }
}

/// Derives an X25519 static secret from an Ed25519 signing key.
///
/// This follows the standard conversion used by libsodium and other libraries:
/// 1. Hash the Ed25519 seed with SHA-512
/// 2. Take the first 32 bytes
/// 3. Apply X25519 clamping (clear bits 0,1,2,255; set bit 254)
fn derive_x25519_from_ed25519(ed25519_key: &Ed25519SigningKey) -> X25519StaticSecret {
    // Get the raw seed bytes
    let seed = ed25519_key.to_bytes();

    // Hash with SHA-512 (same as Ed25519 key expansion)
    let mut hasher = Sha512::new();
    hasher.update(seed);
    let hash = hasher.finalize();

    // Take first 32 bytes and clamp for X25519
    let mut x25519_bytes = [0u8; 32];
    x25519_bytes.copy_from_slice(&hash[..32]);

    // Apply X25519 clamping
    x25519_bytes[0] &= 248;  // Clear bits 0, 1, 2
    x25519_bytes[31] &= 127; // Clear bit 255
    x25519_bytes[31] |= 64;  // Set bit 254

    X25519StaticSecret::from(x25519_bytes)
}

/// Parses key bytes from either raw binary or hex-encoded format.
fn parse_key_bytes(contents: &[u8]) -> Result<[u8; 32], RelayError> {
    // Try as raw 32 bytes first
    if contents.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(contents);
        return Ok(arr);
    }

    // Try as hex (with optional 0x prefix and whitespace)
    let hex_str = std::str::from_utf8(contents)
        .map_err(|_| RelayError::ConfigError("key file is not valid UTF-8 or binary".into()))?
        .trim();

    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);

    let bytes = hex::decode(hex_str)
        .map_err(|e| RelayError::ConfigError(format!("invalid hex in key file: {e}")))?;

    if bytes.len() != 32 {
        return Err(RelayError::ConfigError(format!(
            "key must be 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keypair() {
        let kp = SequencerKeypair::generate();

        // Keys should be non-zero
        assert_ne!(kp.ed25519_public_key(), [0u8; 32]);
        assert_ne!(kp.x25519_public_key(), [0u8; 32]);

        // Ed25519 and X25519 public keys should be different
        assert_ne!(kp.ed25519_public_key(), kp.x25519_public_key());
    }

    #[test]
    fn test_keypair_deterministic() {
        // Same seed should produce same derived keys
        let seed = [42u8; 32];
        let ed25519_key = Ed25519SigningKey::from_bytes(&seed);

        let kp1 = SequencerKeypair::from_ed25519(ed25519_key.clone());
        let kp2 = SequencerKeypair::from_ed25519(ed25519_key);

        assert_eq!(kp1.ed25519_public_key(), kp2.ed25519_public_key());
        assert_eq!(kp1.x25519_public_key(), kp2.x25519_public_key());
        assert_eq!(kp1.x25519_secret_bytes(), kp2.x25519_secret_bytes());
    }

    #[test]
    fn test_x25519_derivation_clamping() {
        let kp = SequencerKeypair::generate();
        let secret_bytes = kp.x25519_secret_bytes();

        // Verify clamping was applied
        assert_eq!(secret_bytes[0] & 7, 0);   // Bits 0,1,2 cleared
        assert_eq!(secret_bytes[31] & 128, 0); // Bit 255 cleared
        assert_eq!(secret_bytes[31] & 64, 64); // Bit 254 set
    }

    #[test]
    fn test_encryption_roundtrip() {
        use crate::crypto::{decrypt, encrypt};

        let kp = SequencerKeypair::generate();
        let plaintext = b"Hello, encrypted relay!";

        // Encrypt with derived X25519 public key
        let ciphertext = encrypt(&kp.x25519_public_key(), plaintext).unwrap();

        // Decrypt with derived X25519 secret key
        let decrypted = decrypt(&kp.x25519_secret_bytes(), &ciphertext).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_parse_key_bytes_raw() {
        let raw = [1u8; 32];
        let parsed = parse_key_bytes(&raw).unwrap();
        assert_eq!(parsed, raw);
    }

    #[test]
    fn test_parse_key_bytes_hex() {
        let expected = [0xab; 32];
        let hex = "ab".repeat(32);

        let parsed = parse_key_bytes(hex.as_bytes()).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn test_parse_key_bytes_hex_with_prefix() {
        let expected = [0xcd; 32];
        let hex = format!("0x{}", "cd".repeat(32));

        let parsed = parse_key_bytes(hex.as_bytes()).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn test_parse_key_bytes_with_whitespace() {
        let expected = [0xef; 32];
        let hex = format!("  {}  \n", "ef".repeat(32));

        let parsed = parse_key_bytes(hex.as_bytes()).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn test_from_hex() {
        let hex_seed = "ab".repeat(32);
        let kp = SequencerKeypair::from_hex(&hex_seed).unwrap();
        assert_ne!(kp.ed25519_public_key(), [0u8; 32]);

        // With 0x prefix
        let hex_with_prefix = format!("0x{}", "cd".repeat(32));
        let kp2 = SequencerKeypair::from_hex(&hex_with_prefix).unwrap();
        assert_ne!(kp2.ed25519_public_key(), [0u8; 32]);

        // Different seeds should give different keys
        assert_ne!(kp.ed25519_public_key(), kp2.ed25519_public_key());
    }

    #[test]
    fn test_parse_key_bytes_invalid_length() {
        let short = [1u8; 16];
        assert!(parse_key_bytes(&short).is_err());

        let long_hex = "ab".repeat(33);
        assert!(parse_key_bytes(long_hex.as_bytes()).is_err());
    }

    #[test]
    fn test_save_and_load() {
        let kp1 = SequencerKeypair::generate();

        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_keypair.key");

        kp1.save_to_file(&path).unwrap();
        let kp2 = SequencerKeypair::load_from_file(&path).unwrap();

        assert_eq!(kp1.ed25519_public_key(), kp2.ed25519_public_key());
        assert_eq!(kp1.x25519_public_key(), kp2.x25519_public_key());

        // Clean up
        let _ = std::fs::remove_file(&path);
    }
}
