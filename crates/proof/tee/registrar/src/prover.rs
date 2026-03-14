//! JSON-RPC client for polling prover instance signer endpoints.

use std::time::Duration;

use alloy_primitives::{Address, Bytes, keccak256};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
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
    /// `endpoint` is a `host:port` string (e.g.
    /// `"prover-0.prover-headless.provers.svc.cluster.local:8000"` in K8s mode
    /// or `"10.0.1.5:8000"` in AWS mode). An `http://` scheme is prepended
    /// automatically.
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
    /// Uses `Vec<u8>` deserialization to match the server's `RpcResult<Vec<u8>>` return type
    /// (serialized as a JSON array of numbers by serde). Note: the existing `EnclaveClient`
    /// in `base-enclave-client` deserializes as `Bytes` (hex string) for the same methods —
    /// if the wire format is confirmed to differ, this should be updated accordingly.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>> {
        debug!(endpoint = %self.endpoint, "fetching signer public key");
        self.inner.request::<Vec<u8>, _>("enclave_signerPublicKey", rpc_params![]).await.map_err(
            |e| RegistrarError::ProverClient {
                instance: self.endpoint.clone(),
                source: Box::new(e),
            },
        )
    }

    /// Fetches the raw Nitro attestation document via `enclave_signerAttestation`.
    pub async fn signer_attestation(&self) -> Result<Vec<u8>> {
        debug!(endpoint = %self.endpoint, "fetching signer attestation");
        self.inner.request::<Vec<u8>, _>("enclave_signerAttestation", rpc_params![]).await.map_err(
            |e| RegistrarError::ProverClient {
                instance: self.endpoint.clone(),
                source: Box::new(e),
            },
        )
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
    use k256::ecdsa::SigningKey;
    use rstest::rstest;

    use super::*;

    /// Well-known Hardhat / Anvil account #0 private key.
    const HARDHAT_PRIVATE_KEY: [u8; 32] = [
        0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff,
        0x94, 0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2,
        0xff, 0x80,
    ];

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
    #[case::compressed(33)]
    #[case::too_long(66)]
    fn derive_address_rejects_wrong_length(#[case] len: usize) {
        let key = vec![0x04; len];
        assert!(ProverClient::derive_address(&key).is_err());
    }

    #[test]
    fn derive_address_rejects_wrong_prefix() {
        let mut key = hardhat_public_key();
        key[0] = 0x02; // compressed prefix
        assert!(ProverClient::derive_address(&key).is_err());
    }
}
