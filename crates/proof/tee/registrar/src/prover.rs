//! JSON-RPC client for polling prover instance signer endpoints.

use std::time::Duration;

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_proof_primitives::EnclaveApiClient;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use k256::ecdsa::VerifyingKey;
use tracing::debug;
use url::Url;

use crate::{EnclaveEndpointClient, RegistrarError, Result};

/// JSON-RPC client for prover instance signer endpoints.
///
/// Implements [`EnclaveEndpointClient`] by making HTTP JSON-RPC calls to the prover's
/// `enclave_signerPublicKey` and `enclave_signerAttestation` endpoints.
///
/// The `timeout` is configured once at construction and applied to all requests.
#[derive(Debug)]
pub struct ProverClient {
    /// Timeout applied to all JSON-RPC requests.
    timeout: Duration,
}

impl ProverClient {
    /// Creates a new client with the given request timeout.
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Builds a `jsonrpsee` HTTP client for `endpoint`.
    ///
    /// The URL must include a scheme (e.g. `http://10.0.1.5:8000`).
    fn build_client(&self, endpoint: &Url) -> Result<HttpClient> {
        HttpClientBuilder::default().request_timeout(self.timeout).build(endpoint.as_str()).map_err(
            |e| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: Box::new(e),
            },
        )
    }

    /// Derives an Ethereum [`Address`] from a SEC1-encoded public key.
    ///
    /// Accepts both compressed (33-byte) and uncompressed (65-byte) SEC1 formats.
    pub fn derive_address(public_key: &[u8]) -> Result<Address> {
        let verifying_key = VerifyingKey::from_sec1_bytes(public_key)
            .map_err(|e| RegistrarError::InvalidPublicKey(e.to_string()))?;
        Ok(public_key_to_address(&verifying_key))
    }
}

impl EnclaveEndpointClient for ProverClient {
    async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
        debug!(endpoint = %endpoint, "fetching signer public keys");
        let client = self.build_client(endpoint)?;
        client.signer_public_key().await.map_err(|e| RegistrarError::ProverClient {
            instance: endpoint.to_string(),
            source: Box::new(e),
        })
    }

    async fn signer_attestation(
        &self,
        endpoint: &Url,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>> {
        debug!(endpoint = %endpoint, "fetching signer attestations");
        let client = self.build_client(endpoint)?;
        client.signer_attestation(None, nonces).await.map_err(|e| RegistrarError::ProverClient {
            instance: endpoint.to_string(),
            source: Box::new(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use hex_literal::hex;

    use super::*;
    use crate::test_utils::{HARDHAT_KEY_0, public_key_from_private};

    const HARDHAT_COMPRESSED_PUBLIC_KEY: [u8; 33] =
        hex!("038318535b54105d4a7aae60c08fc45f9687181b4fdfc625bd1a753fa7397fed75");

    #[test]
    fn derive_address_hardhat_account_zero_public_keys() {
        let uncompressed = public_key_from_private(&HARDHAT_KEY_0);

        for public_key in [uncompressed.as_slice(), HARDHAT_COMPRESSED_PUBLIC_KEY.as_slice()] {
            let derived = ProverClient::derive_address(public_key).unwrap();
            assert_eq!(derived, address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
        }
    }

    #[test]
    fn derive_address_rejects_invalid_bytes() {
        let mut prefix_length_mismatch = public_key_from_private(&HARDHAT_KEY_0);
        prefix_length_mismatch[0] = 0x02;

        for key in [vec![], vec![0x04; 33], vec![0x04; 66], prefix_length_mismatch] {
            assert!(ProverClient::derive_address(&key).is_err());
        }
    }
}
