//! JSON-RPC client for polling prover instance signer endpoints.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use alloy_primitives::{Address, keccak256};
use async_trait::async_trait;
use base_proof_primitives::EnclaveApiClient;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use tracing::debug;
use url::Url;

use crate::{RegistrarError, Result, SignerClient};

/// JSON-RPC client for prover instance signer endpoints.
///
/// Implements [`SignerClient`] by making HTTP JSON-RPC calls to the prover's
/// `enclave_signerPublicKey` and `enclave_signerAttestation` endpoints.
///
/// HTTP clients are cached per endpoint so that TCP connections are reused
/// across poll cycles. The `timeout` is configured once at construction and
/// applied to all requests.
///
/// Also provides [`derive_address`](Self::derive_address), a pure-crypto helper
/// that derives an Ethereum address from a SEC1-encoded public key.
pub struct ProverClient {
    /// Timeout applied to all JSON-RPC requests.
    timeout: Duration,
    /// Cached HTTP clients keyed by endpoint URL.
    ///
    /// Note: this cache is append-only — entries are never evicted. With
    /// ephemeral instances (e.g. ASG scale events assigning new IPs), stale
    /// entries accumulate over the process lifetime. The fleet is small enough
    /// that this is not a practical concern; if it ever becomes one, a
    /// `retain_active` method could prune entries not in the current discovered
    /// set after each poll cycle.
    clients: Mutex<HashMap<Url, HttpClient>>,
}

impl std::fmt::Debug for ProverClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProverClient")
            .field("timeout", &self.timeout)
            .field("cached_clients", &self.clients.lock().map(|c| c.len()).unwrap_or(0))
            .finish()
    }
}

impl ProverClient {
    /// Creates a new client with the given request timeout.
    pub fn new(timeout: Duration) -> Self {
        Self { timeout, clients: Mutex::new(HashMap::new()) }
    }

    /// Returns a cached `jsonrpsee` HTTP client for `endpoint`, building one
    /// on first access.
    ///
    /// The URL must include a scheme (e.g. `http://10.0.1.5:8000`).
    fn get_or_build_client(&self, endpoint: &Url) -> Result<HttpClient> {
        let mut cache = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = cache.get(endpoint) {
            return Ok(client.clone());
        }
        let client = HttpClientBuilder::default()
            .request_timeout(self.timeout)
            .build(endpoint.as_str())
            .map_err(|e| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: Box::new(e),
            })?;
        cache.insert(endpoint.clone(), client.clone());
        Ok(client)
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
}

#[async_trait]
impl SignerClient for ProverClient {
    async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
        debug!(endpoint = %endpoint, "fetching signer public keys");
        let client = self.get_or_build_client(endpoint)?;
        client.signer_public_key().await.map_err(|e| RegistrarError::ProverClient {
            instance: endpoint.to_string(),
            source: Box::new(e),
        })
    }

    async fn signer_attestation(
        &self,
        endpoint: &Url,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>> {
        debug!(endpoint = %endpoint, "fetching signer attestations");
        let client = self.get_or_build_client(endpoint)?;
        client.signer_attestation(user_data, nonce).await.map_err(|e| {
            RegistrarError::ProverClient { instance: endpoint.to_string(), source: Box::new(e) }
        })
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
