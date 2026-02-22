//! Enclave RPC client.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee::rpc_params;
use serde::{Deserialize, Serialize};

use op_enclave_core::{ExecuteStatelessRequest, Proposal};

use crate::client_error::ClientError;

/// Module for insecure TLS configuration that skips certificate verification.
mod danger {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::ED25519,
            ]
        }
    }
}

/// Request for the `aggregate` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AggregateRequest {
    config_hash: B256,
    prev_output_root: B256,
    prev_block_number: u64,
    proposals: Vec<Proposal>,
    proposer: Address,
    tee_image_hash: B256,
    #[serde(default)]
    intermediate_roots: Vec<B256>,
}

/// Client for the enclave RPC server.
#[derive(Debug, Clone)]
pub struct EnclaveClient {
    inner: HttpClient,
}

impl EnclaveClient {
    /// Create a new enclave client.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the enclave RPC server (e.g., `http://127.0.0.1:1234`)
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn new(url: &str) -> Result<Self, ClientError> {
        let inner = HttpClientBuilder::default()
            .build(url)
            .map_err(|e| ClientError::ClientCreation(e.to_string()))?;

        Ok(Self { inner })
    }

    /// Create a new enclave client with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the enclave RPC server
    /// * `max_request_size` - Maximum request body size in bytes
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn with_config(url: &str, max_request_size: u32) -> Result<Self, ClientError> {
        let inner = HttpClientBuilder::default()
            .max_request_size(max_request_size)
            .build(url)
            .map_err(|e| ClientError::ClientCreation(e.to_string()))?;

        Ok(Self { inner })
    }

    /// Create a new enclave client with TLS verification optionally disabled.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the enclave RPC server
    /// * `max_request_size` - Maximum request body size in bytes
    /// * `skip_tls_verify` - Whether to skip TLS certificate verification
    ///
    /// # Warning
    ///
    /// Setting `skip_tls_verify` to `true` disables TLS certificate verification,
    /// making the connection vulnerable to man-in-the-middle attacks.
    /// This should ONLY be used in development/testing environments.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn with_tls_config(
        url: &str,
        max_request_size: u32,
        skip_tls_verify: bool,
    ) -> Result<Self, ClientError> {
        let builder = HttpClientBuilder::default().max_request_size(max_request_size);

        let inner = if skip_tls_verify {
            let tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(danger::NoCertificateVerification))
                .with_no_client_auth();

            builder
                .with_custom_cert_store(tls_config)
                .build(url)
                .map_err(|e| ClientError::ClientCreation(e.to_string()))?
        } else {
            builder
                .build(url)
                .map_err(|e| ClientError::ClientCreation(e.to_string()))?
        };

        Ok(Self { inner })
    }

    /// Get the signer's public key as a 65-byte uncompressed EC point.
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails.
    pub async fn signer_public_key(&self) -> Result<Bytes, ClientError> {
        self.inner
            .request("enclave_signerPublicKey", rpc_params![])
            .await
            .map_err(Into::into)
    }

    /// Get an attestation document containing the signer's public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails or if running in local mode.
    pub async fn signer_attestation(&self) -> Result<Bytes, ClientError> {
        self.inner
            .request("enclave_signerAttestation", rpc_params![])
            .await
            .map_err(Into::into)
    }

    /// Get the decryption public key in PKIX/DER format.
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails.
    pub async fn decryption_public_key(&self) -> Result<Bytes, ClientError> {
        self.inner
            .request("enclave_decryptionPublicKey", rpc_params![])
            .await
            .map_err(Into::into)
    }

    /// Get an attestation document containing the decryption public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails or if running in local mode.
    pub async fn decryption_attestation(&self) -> Result<Bytes, ClientError> {
        self.inner
            .request("enclave_decryptionAttestation", rpc_params![])
            .await
            .map_err(Into::into)
    }

    /// Encrypt the signer key for a remote enclave.
    ///
    /// # Arguments
    ///
    /// * `attestation` - The attestation document from the remote enclave
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails.
    pub async fn encrypted_signer_key(&self, attestation: Bytes) -> Result<Bytes, ClientError> {
        self.inner
            .request("enclave_encryptedSignerKey", rpc_params![attestation])
            .await
            .map_err(Into::into)
    }

    /// Set the signer key from an encrypted key.
    ///
    /// # Arguments
    ///
    /// * `encrypted` - The encrypted signer key
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails.
    pub async fn set_signer_key(&self, encrypted: Bytes) -> Result<(), ClientError> {
        self.inner
            .request("enclave_setSignerKey", rpc_params![encrypted])
            .await
            .map_err(Into::into)
    }

    /// Execute stateless block validation and create a signed proposal.
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails or block validation fails.
    pub async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError> {
        self.inner
            .request("enclave_executeStateless", rpc_params![req])
            .await
            .map_err(Into::into)
    }

    /// Aggregate multiple proposals into a single proposal.
    ///
    /// # Arguments
    ///
    /// * `config_hash` - The per-chain configuration hash
    /// * `prev_output_root` - The output root before the first proposal
    /// * `prev_block_number` - The L2 block number before the first proposal
    /// * `proposals` - The proposals to aggregate
    /// * `proposer` - The proposer address for the signed journal
    /// * `tee_image_hash` - The TEE image hash for the signed journal
    /// * `intermediate_roots` - Intermediate output roots at every `intermediate_block_interval`
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails or signature verification fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn aggregate(
        &self,
        config_hash: B256,
        prev_output_root: B256,
        prev_block_number: u64,
        proposals: Vec<Proposal>,
        proposer: Address,
        tee_image_hash: B256,
        intermediate_roots: Vec<B256>,
    ) -> Result<Proposal, ClientError> {
        let request = AggregateRequest {
            config_hash,
            prev_output_root,
            prev_block_number,
            proposals,
            proposer,
            tee_image_hash,
            intermediate_roots,
        };

        self.inner
            .request("enclave_aggregate", rpc_params![request])
            .await
            .map_err(Into::into)
    }
}
