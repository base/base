//! Enclave server implementation.
//!
//! This module contains the main [`Server`] struct that manages cryptographic
//! keys and attestation for the enclave.

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use parking_lot::RwLock;
use rsa::RsaPrivateKey;

use crate::attestation::{extract_public_key, verify_attestation_with_pcr0};
use crate::crypto::{
    decrypt_pkcs1v15, encrypt_pkcs1v15, generate_rsa_key, generate_signer, pkix_to_public_key,
    private_key_bytes, private_to_public, public_key_bytes, public_key_to_pkix, signer_from_bytes,
    signer_from_hex,
};
use crate::error::{CryptoError, NsmError, ServerError};
use crate::nsm::{NsmRng, NsmSession};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "OP_ENCLAVE_SIGNER_KEY";

/// The enclave server.
///
/// Manages cryptographic keys and attestation for the enclave.
/// Supports both Nitro Enclave mode (with NSM) and local mode (for development).
#[derive(Debug)]
pub struct Server {
    /// PCR0 measurement (empty in local mode).
    pcr0: Vec<u8>,
    /// ECDSA signing key with interior mutability for `set_signer_key`.
    signer_key: RwLock<PrivateKeySigner>,
    /// RSA-4096 key for decrypting received signer keys.
    decryption_key: RsaPrivateKey,
}

impl Server {
    /// Create a new server instance.
    ///
    /// This attempts to open an NSM session. If successful, it reads PCR0
    /// and uses NSM for random number generation. If NSM is unavailable,
    /// it falls back to local mode and optionally reads the signer key
    /// from the `OP_ENCLAVE_SIGNER_KEY` environment variable.
    pub fn new() -> Result<Self, ServerError> {
        let (mut rng, pcr0, signer_key_env) = match NsmSession::open()? {
            Some(session) => {
                let pcr0 = session.describe_pcr0()?;
                let rng = NsmRng::new().ok_or_else(|| {
                    NsmError::SessionOpen("failed to initialize NSM RNG".to_string())
                })?;
                (rng, pcr0, None)
            }
            None => {
                tracing::warn!("running in local mode without NSM");
                let rng = NsmRng::default();
                let signer_key_env = std::env::var(SIGNER_KEY_ENV_VAR).ok();
                (rng, Vec::new(), signer_key_env)
            }
        };

        // Generate RSA decryption key
        let decryption_key = generate_rsa_key(&mut rng)?;

        // Generate or load ECDSA signer key
        let signer_key = if let Some(hex_key) = signer_key_env {
            tracing::info!("using signer key from environment variable");
            signer_from_hex(&hex_key)?
        } else {
            generate_signer(&mut rng)?
        };

        let local_mode = pcr0.is_empty();
        tracing::info!(
            address = %signer_key.address(),
            local_mode = local_mode,
            "server initialized"
        );

        Ok(Self {
            pcr0,
            signer_key: RwLock::new(signer_key),
            decryption_key,
        })
    }

    /// Check if the server is running in local mode.
    #[must_use]
    pub const fn is_local_mode(&self) -> bool {
        self.pcr0.is_empty()
    }

    /// Get the PCR0 measurement.
    ///
    /// Returns an empty vector in local mode.
    #[must_use]
    pub fn pcr0(&self) -> &[u8] {
        &self.pcr0
    }

    /// Get the signer's public key as a 65-byte uncompressed EC point.
    ///
    /// This matches Go's `crypto.FromECDSAPub()` format.
    #[must_use]
    pub fn signer_public_key(&self) -> Vec<u8> {
        let signer = self.signer_key.read();
        public_key_bytes(&signer)
    }

    /// Get the signer's Ethereum address.
    #[must_use]
    pub fn signer_address(&self) -> Address {
        let signer = self.signer_key.read();
        signer.address()
    }

    /// Get an attestation document containing the signer's public key.
    ///
    /// Returns an error in local mode.
    pub fn signer_attestation(&self) -> Result<Vec<u8>, ServerError> {
        self.public_key_attestation(|| Ok(self.signer_public_key()))
    }

    /// Get the decryption public key in PKIX/DER format.
    ///
    /// This matches Go's `x509.MarshalPKIXPublicKey()` format.
    pub fn decryption_public_key(&self) -> Result<Vec<u8>, ServerError> {
        let public_key = private_to_public(&self.decryption_key);
        public_key_to_pkix(&public_key)
    }

    /// Get an attestation document containing the decryption public key.
    ///
    /// Returns an error in local mode.
    pub fn decryption_attestation(&self) -> Result<Vec<u8>, ServerError> {
        self.public_key_attestation(|| self.decryption_public_key())
    }

    /// Get an attestation document containing the given public key.
    fn public_key_attestation<F>(&self, public_key_fn: F) -> Result<Vec<u8>, ServerError>
    where
        F: FnOnce() -> Result<Vec<u8>, ServerError>,
    {
        let session = NsmSession::open()?
            .ok_or_else(|| NsmError::SessionOpen("NSM not available".to_string()))?;

        let public_key = public_key_fn()?;
        session.get_attestation(public_key)
    }

    /// Encrypt the signer key for a remote enclave.
    ///
    /// This verifies the provided attestation document, checks that PCR0 matches,
    /// and encrypts the signer key using the public key from the attestation.
    ///
    /// # Arguments
    /// * `attestation` - The attestation document from the remote enclave
    ///
    /// # Returns
    /// The encrypted signer key (can be decrypted with `set_signer_key`)
    pub fn encrypted_signer_key(&self, attestation: &[u8]) -> Result<Vec<u8>, ServerError> {
        // Verify attestation and check PCR0
        let result = verify_attestation_with_pcr0(attestation, &self.pcr0)?;

        // Extract and parse the public key
        let public_key_der = extract_public_key(&result.document)?;
        let public_key = pkix_to_public_key(&public_key_der)?;

        // Get the signer's private key
        let signer = self.signer_key.read();
        let signer_private_key = private_key_bytes(&signer);

        // Encrypt with NSM RNG
        let mut rng = NsmRng::default();
        encrypt_pkcs1v15(&mut rng, &public_key, &signer_private_key)
    }

    /// Set the signer key from an encrypted key.
    ///
    /// This decrypts the provided ciphertext using the decryption key
    /// and updates the signer key.
    ///
    /// # Arguments
    /// * `encrypted` - The encrypted signer key (from `encrypted_signer_key`)
    pub fn set_signer_key(&self, encrypted: &[u8]) -> Result<(), ServerError> {
        let decrypted = decrypt_pkcs1v15(&self.decryption_key, encrypted)?;

        if decrypted.len() != 32 {
            return Err(CryptoError::InvalidPrivateKeyLength(decrypted.len()).into());
        }

        let new_signer = signer_from_bytes(&decrypted)?;

        tracing::info!(
            address = %new_signer.address(),
            "updated signer key"
        );

        *self.signer_key.write() = new_signer;
        Ok(())
    }

    /// Get a read guard for the signer.
    ///
    /// Use this for signing operations.
    pub fn signer(&self) -> parking_lot::RwLockReadGuard<'_, PrivateKeySigner> {
        self.signer_key.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_new_local_mode() {
        // On non-Linux or without NSM, should create in local mode
        let server = Server::new().expect("failed to create server");

        #[cfg(not(target_os = "linux"))]
        assert!(server.is_local_mode());

        // Should have a valid signer key
        let public_key = server.signer_public_key();
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    fn test_decryption_public_key() {
        let server = Server::new().expect("failed to create server");
        let public_key = server.decryption_public_key().expect("failed to get key");
        // PKIX-encoded RSA-4096 public key should be around 550 bytes
        assert!(public_key.len() > 500);
    }

    #[test]
    #[ignore = "slow: generates two RSA-4096 keys"]
    fn test_key_exchange_between_servers() {
        // Simulate key exchange between two local servers
        let server1 = Server::new().expect("failed to create server1");
        let server2 = Server::new().expect("failed to create server2");

        // In local mode, we can't use attestation, but we can test the RSA path
        let signer = server1.signer();
        let private_key = private_key_bytes(&signer);
        drop(signer);

        // Encrypt the key with server2's decryption key
        let public_key = server2.decryption_public_key().expect("failed to get key");
        let parsed_key = pkix_to_public_key(&public_key).expect("failed to parse key");

        let mut rng = rand::rngs::OsRng;
        let encrypted =
            encrypt_pkcs1v15(&mut rng, &parsed_key, &private_key).expect("failed to encrypt");

        // Decrypt and set on server2
        server2
            .set_signer_key(&encrypted)
            .expect("failed to set key");

        // Verify both servers now have the same signer address
        assert_eq!(server1.signer_address(), server2.signer_address());
    }

    #[test]
    fn test_signer_address_consistency() {
        let server = Server::new().expect("failed to create server");

        // Address should be consistent across calls
        let addr1 = server.signer_address();
        let addr2 = server.signer_address();
        assert_eq!(addr1, addr2);

        // Public key should also be consistent
        let pk1 = server.signer_public_key();
        let pk2 = server.signer_public_key();
        assert_eq!(pk1, pk2);
    }
}
