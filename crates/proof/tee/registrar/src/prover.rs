//! JSON-RPC client for polling prover instance signer endpoints.

use std::time::Duration;

use alloy_primitives::{Address, Bytes, keccak256};
use base_proof_primitives::EnclaveApiClient;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use tracing::debug;

use crate::{AttestationResponse, RegistrarError, Result};

/// JSON-RPC client for a single prover instance's signer endpoints.
///
/// Wraps a `jsonrpsee` HTTP client configured for the prover's endpoint.
/// Calls `enclave_signerPublicKey` and `enclave_signerAttestation` to obtain
/// the signer's public key and Nitro attestation document, then derives the
/// Ethereum address from the uncompressed public key.
#[derive(Debug)]
pub struct ProverClient {
    /// The raw endpoint string (host:port, without scheme).
    endpoint: String,
    /// The underlying `jsonrpsee` HTTP client.
    inner: HttpClient,
}

impl ProverClient {
    /// Creates a new client for the prover at the given endpoint.
    ///
    /// `endpoint` is a `host:port` string (e.g. `"10.0.1.5:8000"`).
    /// An `http://` scheme is prepended automatically.
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self> {
        let url = format!("http://{endpoint}");
        let inner =
            HttpClientBuilder::default().request_timeout(timeout).build(&url).map_err(|e| {
                RegistrarError::ProverClient { instance: endpoint.to_string(), source: Box::new(e) }
            })?;
        Ok(Self { endpoint: endpoint.to_string(), inner })
    }

    /// Fetches the signer's 65-byte uncompressed public key via `enclave_signerPublicKey`.
    ///
    /// Uses the generated [`EnclaveApiClient`] trait from `base-proof-primitives`,
    /// which ensures the method name and return type (`Vec<u8>`) match the server's
    /// `EnclaveApi` trait at compile time.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>> {
        debug!(endpoint = %self.endpoint, "fetching signer public key");
        self.inner.signer_public_key().await.map_err(|e| RegistrarError::ProverClient {
            instance: self.endpoint.clone(),
            source: Box::new(e),
        })
    }

    /// Fetches the raw Nitro attestation document via `enclave_signerAttestation`.
    pub async fn signer_attestation(&self) -> Result<Vec<u8>> {
        debug!(endpoint = %self.endpoint, "fetching signer attestation");
        self.inner.signer_attestation().await.map_err(|e| RegistrarError::ProverClient {
            instance: self.endpoint.clone(),
            source: Box::new(e),
        })
    }

    /// Derives an Ethereum [`Address`] from a SEC1-encoded public key.
    ///
    /// Validates that the bytes represent a valid point on the secp256k1 curve
    /// (via [`k256::PublicKey::from_sec1_bytes`]), then computes the address as
    /// `keccak256(uncompressed_point[1..])[12..]`.
    ///
    /// Accepts both compressed (33-byte) and uncompressed (65-byte) SEC1 formats.
    pub fn derive_address(public_key: &[u8]) -> Result<Address> {
        let key = k256::PublicKey::from_sec1_bytes(public_key)
            .map_err(|e| RegistrarError::InvalidPublicKey(e.to_string()))?;
        let uncompressed = key.to_encoded_point(false);
        let hash = keccak256(&uncompressed.as_bytes()[1..]);
        Ok(Address::from_slice(&hash[12..]))
    }

    /// Fetches the signer public key and attestation, derives the Ethereum address,
    /// and returns a combined [`AttestationResponse`].
    ///
    /// This is the primary entry point for the registration poll loop. Calls
    /// `enclave_signerPublicKey` first to get the address (cheap), then
    /// `enclave_signerAttestation` for the raw attestation document (involves
    /// Nitro NSM hardware — only call when registration is needed).
    pub async fn get_attestation_response(&self) -> Result<AttestationResponse> {
        let public_key = self.signer_public_key().await?;
        let signer_address = Self::derive_address(&public_key)?;
        let attestation = self.signer_attestation().await?;

        debug!(
            endpoint = %self.endpoint,
            signer_address = %signer_address,
            "fetched attestation response"
        );

        Ok(AttestationResponse { attestation_bytes: Bytes::from(attestation), signer_address })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use hex_literal::hex;
    use k256::ecdsa::SigningKey;
    use rstest::rstest;

    use super::*;

    /// Well-known Hardhat / Anvil account #0 private key.
    const HARDHAT_PRIVATE_KEY: [u8; 32] =
        hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    /// Returns the 65-byte uncompressed public key for the Hardhat account #0.
    fn hardhat_public_key() -> Vec<u8> {
        let signing_key = SigningKey::from_slice(&HARDHAT_PRIVATE_KEY).unwrap();
        let verifying_key = signing_key.verifying_key();
        verifying_key.to_encoded_point(false).as_bytes().to_vec()
    }

    #[test]
    fn derive_address_hardhat_account_zero() {
        let public_key = hardhat_public_key();
        let derived = ProverClient::derive_address(&public_key).unwrap();
        assert_eq!(derived, address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
    }

    #[test]
    fn derive_address_validates_correct_format() {
        let public_key = hardhat_public_key();
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
        assert!(ProverClient::derive_address(&public_key).is_ok());
    }

    #[rstest]
    #[case::empty(0)]
    #[case::invalid_33_bytes(33)]
    #[case::too_long(66)]
    fn derive_address_rejects_invalid_bytes(#[case] len: usize) {
        let key = vec![0x04; len];
        assert!(ProverClient::derive_address(&key).is_err());
    }

    #[test]
    fn derive_address_rejects_prefix_length_mismatch() {
        // 0x02 = compressed prefix expects 33 bytes, but key is 65 bytes.
        let mut key = hardhat_public_key();
        key[0] = 0x02;
        assert!(ProverClient::derive_address(&key).is_err());
    }

    #[test]
    fn derive_address_compressed_matches_uncompressed() {
        let signing_key = SigningKey::from_slice(&HARDHAT_PRIVATE_KEY).unwrap();
        let verifying_key = signing_key.verifying_key();
        let compressed = verifying_key.to_encoded_point(true).as_bytes().to_vec();
        let uncompressed = hardhat_public_key();

        let addr_compressed = ProverClient::derive_address(&compressed).unwrap();
        let addr_uncompressed = ProverClient::derive_address(&uncompressed).unwrap();
        assert_eq!(addr_compressed, addr_uncompressed);
    }
}
