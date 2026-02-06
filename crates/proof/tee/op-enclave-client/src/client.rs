//! Enclave RPC client.

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_primitives::{B256, Bytes};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee::rpc_params;
use serde::{Deserialize, Serialize};

use op_enclave_core::Proposal;
use op_enclave_core::executor::ExecutionWitness;
use op_enclave_core::types::account::AccountResult;
use op_enclave_core::types::config::PerChainConfig;

use crate::client_error::ClientError;

/// Request for the `executeStateless` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteStatelessRequest {
    config: PerChainConfig,
    l1_origin: Header,
    l1_receipts: Vec<ReceiptEnvelope>,
    previous_block_txs: Vec<Bytes>,
    block_header: Header,
    sequenced_txs: Vec<Bytes>,
    witness: ExecutionWitness,
    message_account: AccountResult,
    prev_message_account_hash: B256,
}

/// Request for the `aggregate` RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AggregateRequest {
    config_hash: B256,
    prev_output_root: B256,
    proposals: Vec<Proposal>,
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
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_stateless(
        &self,
        config: PerChainConfig,
        l1_origin: Header,
        l1_receipts: Vec<ReceiptEnvelope>,
        previous_block_txs: Vec<Bytes>,
        block_header: Header,
        sequenced_txs: Vec<Bytes>,
        witness: ExecutionWitness,
        message_account: AccountResult,
        prev_message_account_hash: B256,
    ) -> Result<Proposal, ClientError> {
        let request = ExecuteStatelessRequest {
            config,
            l1_origin,
            l1_receipts,
            previous_block_txs,
            block_header,
            sequenced_txs,
            witness,
            message_account,
            prev_message_account_hash,
        };

        self.inner
            .request("enclave_executeStateless", rpc_params![request])
            .await
            .map_err(Into::into)
    }

    /// Aggregate multiple proposals into a single proposal.
    ///
    /// # Arguments
    ///
    /// * `config_hash` - The per-chain configuration hash
    /// * `prev_output_root` - The output root before the first proposal
    /// * `proposals` - The proposals to aggregate
    ///
    /// # Errors
    ///
    /// Returns an error if the RPC call fails or signature verification fails.
    pub async fn aggregate(
        &self,
        config_hash: B256,
        prev_output_root: B256,
        proposals: Vec<Proposal>,
    ) -> Result<Proposal, ClientError> {
        let request = AggregateRequest {
            config_hash,
            prev_output_root,
            proposals,
        };

        self.inner
            .request("enclave_aggregate", rpc_params![request])
            .await
            .map_err(Into::into)
    }
}
