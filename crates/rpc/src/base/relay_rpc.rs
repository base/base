//! RPC implementation for encrypted transaction relay.

use std::sync::Arc;

use alloy_primitives::{B256, Bytes, TxHash};
use base_reth_relay::{
    RelayConfigCache, RelayError, pow, crypto,
    p2p::{ForwardRequest, ForwardResult},
    types::{
        EncryptedTransactionRequest, EncryptedTransactionResponse, RelayParameters,
        MAX_ENCRYPTED_PAYLOAD_SIZE, MIN_ENCRYPTED_PAYLOAD_SIZE,
    },
};
use jsonrpsee::core::{RpcResult, async_trait};
use reth_rpc_eth_api::helpers::EthTransactions;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn, info};

use crate::EncryptedRelayApiServer;

/// HTTP client for forwarding to sequencer.
#[derive(Clone)]
pub struct HttpForwarder {
    /// The sequencer's RPC URL.
    sequencer_url: String,
    /// HTTP client.
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpForwarder")
            .field("sequencer_url", &self.sequencer_url)
            .finish()
    }
}

impl HttpForwarder {
    /// Creates a new HTTP forwarder.
    pub fn new(sequencer_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to create HTTP client");
        Self { sequencer_url, client }
    }

    /// Forwards an encrypted transaction to the sequencer.
    pub async fn forward(&self, request: &EncryptedTransactionRequest) -> Result<EncryptedTransactionResponse, String> {
        #[derive(serde::Serialize)]
        struct JsonRpcRequest<'a> {
            jsonrpc: &'static str,
            method: &'static str,
            params: [&'a EncryptedTransactionRequest; 1],
            id: u64,
        }

        #[derive(serde::Deserialize)]
        struct JsonRpcResponse {
            result: Option<EncryptedTransactionResponse>,
            error: Option<JsonRpcError>,
        }

        #[derive(serde::Deserialize)]
        struct JsonRpcError {
            message: String,
        }

        let rpc_request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "base_sendEncryptedTransaction",
            params: [request],
            id: 1,
        };

        let response = self.client
            .post(&self.sequencer_url)
            .json(&rpc_request)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        let json_response: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;

        if let Some(error) = json_response.error {
            return Err(format!("RPC error: {}", error.message));
        }

        json_response.result.ok_or_else(|| "No result in response".to_string())
    }
}

/// P2P forwarder for sending transactions over the devp2p subprotocol.
#[derive(Clone)]
pub struct P2pForwarder {
    /// Channel to send forward requests to the RelayForwarder task.
    forward_tx: mpsc::Sender<ForwardRequest>,
}

impl std::fmt::Debug for P2pForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pForwarder").finish()
    }
}

impl P2pForwarder {
    /// Creates a new P2P forwarder.
    pub fn new(forward_tx: mpsc::Sender<ForwardRequest>) -> Self {
        Self { forward_tx }
    }

    /// Forwards an encrypted transaction to the sequencer via P2P.
    pub async fn forward(&self, request: &EncryptedTransactionRequest) -> Result<EncryptedTransactionResponse, String> {
        // Compute commitment for tracking
        let commitment = B256::from(pow::commitment(&request.encrypted_payload));

        // Create the forward request
        let (response_tx, response_rx) = oneshot::channel();
        let forward_req = ForwardRequest {
            request: request.clone(),
            response_tx,
        };

        // Send to the forwarder task
        self.forward_tx
            .send(forward_req)
            .await
            .map_err(|_| "P2P forwarder channel closed".to_string())?;

        // Wait for response
        let result = response_rx
            .await
            .map_err(|_| "P2P forwarder response channel dropped".to_string())?;

        // Convert ForwardResult to EncryptedTransactionResponse
        match result {
            ForwardResult::Sent { commitment: result_commitment } => {
                Ok(EncryptedTransactionResponse {
                    accepted: true,
                    commitment: result_commitment,
                    error: None,
                })
            }
            ForwardResult::NoSequencers => {
                Ok(EncryptedTransactionResponse {
                    accepted: false,
                    commitment,
                    error: Some("No sequencer peers connected".to_string()),
                })
            }
            ForwardResult::NetworkError(e) => {
                Ok(EncryptedTransactionResponse {
                    accepted: false,
                    commitment,
                    error: Some(format!("Network error: {e}")),
                })
            }
        }
    }
}

/// Marker type for relay mode (no pool submission).
#[derive(Debug, Clone, Copy)]
pub struct NoPoolSubmission;

/// Implementation of the encrypted relay RPC API.
///
/// Generic over `Eth` which provides access to `send_raw_transaction` for pool submission.
/// Use `NoPoolSubmission` for relay/follower mode, or a real Eth API type for sequencer mode.
pub struct EncryptedRelayApiImpl<Eth> {
    /// Cached relay configuration.
    config: Arc<RelayConfigCache>,
    /// X25519 secret key for decryption (sequencer mode only).
    decryption_key: Option<[u8; 32]>,
    /// HTTP forwarder for relay mode.
    http_forwarder: Option<HttpForwarder>,
    /// P2P forwarder for relay mode.
    p2p_forwarder: Option<P2pForwarder>,
    /// Eth API for submitting transactions (sequencer mode only).
    eth_api: Option<Eth>,
}

impl<Eth> std::fmt::Debug for EncryptedRelayApiImpl<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedRelayApiImpl")
            .field("config", &self.config)
            .field("decryption_key", &self.decryption_key.as_ref().map(|_| "<key>"))
            .field("http_forwarder", &self.http_forwarder)
            .field("p2p_forwarder", &self.p2p_forwarder)
            .field("eth_api", &self.eth_api.as_ref().map(|_| "<eth_api>"))
            .finish()
    }
}

impl EncryptedRelayApiImpl<NoPoolSubmission> {
    /// Creates a new encrypted relay API instance without forwarding or decryption.
    ///
    /// Use this for basic relay nodes.
    pub fn new(config: Arc<RelayConfigCache>) -> Self {
        Self { config, decryption_key: None, http_forwarder: None, p2p_forwarder: None, eth_api: None }
    }

    /// Creates a new encrypted relay API instance with HTTP forwarding.
    ///
    /// Use this for relay nodes that forward to the sequencer via HTTP.
    pub fn with_http_forwarder(config: Arc<RelayConfigCache>, sequencer_url: String) -> Self {
        let forwarder = HttpForwarder::new(sequencer_url);
        Self { config, decryption_key: None, http_forwarder: Some(forwarder), p2p_forwarder: None, eth_api: None }
    }

    /// Creates a new encrypted relay API instance with P2P forwarding.
    ///
    /// Use this for relay nodes that forward to the sequencer via P2P.
    pub fn with_p2p_forwarder(config: Arc<RelayConfigCache>, forward_tx: mpsc::Sender<ForwardRequest>) -> Self {
        let forwarder = P2pForwarder::new(forward_tx);
        Self { config, decryption_key: None, http_forwarder: None, p2p_forwarder: Some(forwarder), eth_api: None }
    }
}

impl<Eth> EncryptedRelayApiImpl<Eth> {
    /// Creates a new encrypted relay API instance with decryption and pool submission.
    ///
    /// Use this for sequencer nodes that decrypt and submit transactions to the pool.
    pub fn with_eth_api(
        config: Arc<RelayConfigCache>,
        decryption_key: [u8; 32],
        eth_api: Eth,
    ) -> Self {
        Self {
            config,
            decryption_key: Some(decryption_key),
            http_forwarder: None,
            p2p_forwarder: None,
            eth_api: Some(eth_api),
        }
    }

    /// Validates an encrypted transaction request.
    fn validate_request(&self, request: &EncryptedTransactionRequest) -> Result<(), RelayError> {
        let payload_len = request.encrypted_payload.len();

        // Check payload size
        if payload_len > MAX_ENCRYPTED_PAYLOAD_SIZE {
            return Err(RelayError::PayloadTooLarge {
                size: payload_len,
                max: MAX_ENCRYPTED_PAYLOAD_SIZE,
            });
        }

        if payload_len < MIN_ENCRYPTED_PAYLOAD_SIZE {
            return Err(RelayError::PayloadTooSmall {
                size: payload_len,
                min: MIN_ENCRYPTED_PAYLOAD_SIZE,
            });
        }

        // Check encryption key is valid
        if !self.config.is_valid_encryption_key(&request.encryption_pubkey) {
            return Err(RelayError::InvalidEncryptionKey);
        }

        // Verify proof-of-work
        let params = self.config.get();
        pow::verify(&request.encrypted_payload, request.pow_nonce, params.pow_difficulty)?;

        Ok(())
    }
}

impl<Eth> EncryptedRelayApiImpl<Eth>
where
    Eth: EthTransactions + Send + Sync + 'static,
{
    /// Attempts to decrypt and submit the transaction to the pool (sequencer mode).
    /// Returns (decrypted_bytes, optional_tx_hash).
    async fn decrypt_and_submit(&self, request: &EncryptedTransactionRequest, commitment: B256) -> Result<(Bytes, Option<TxHash>), RelayError> {
        let decryption_key = self.decryption_key.ok_or_else(|| {
            RelayError::Internal("decryption key not configured".into())
        })?;

        // Decrypt the payload
        let decrypted = crypto::decrypt(&decryption_key, &request.encrypted_payload)?;
        let decrypted_bytes = Bytes::from(decrypted.clone());

        info!(
            commitment = %commitment,
            decrypted_size = decrypted.len(),
            decrypted_hex = %alloy_primitives::hex::encode(&decrypted[..decrypted.len().min(100)]),
            "Decrypted encrypted transaction"
        );

        // Submit to transaction pool via EthTransactions::send_raw_transaction
        let tx_hash = if let Some(ref eth_api) = self.eth_api {
            match EthTransactions::send_raw_transaction(eth_api, decrypted_bytes.clone()).await {
                Ok(hash) => {
                    info!(
                        commitment = %commitment,
                        tx_hash = %hash,
                        "Submitted decrypted transaction to pool"
                    );
                    Some(hash)
                }
                Err(e) => {
                    // Log but don't fail - the payload may not be a valid signed tx
                    warn!(
                        commitment = %commitment,
                        error = %e,
                        "Failed to submit decrypted transaction to pool (may not be a valid tx)"
                    );
                    None
                }
            }
        } else {
            debug!(
                commitment = %commitment,
                "No eth_api configured, skipping pool submission"
            );
            None
        };

        Ok((decrypted_bytes, tx_hash))
    }
}

/// Implementation for non-Eth API modes (HTTP forwarding, legacy mode).
#[async_trait]
impl EncryptedRelayApiServer for EncryptedRelayApiImpl<NoPoolSubmission> {
    async fn send_encrypted_transaction(
        &self,
        request: EncryptedTransactionRequest,
    ) -> RpcResult<EncryptedTransactionResponse> {
        // Compute commitment hash for tracking
        let commitment = B256::from(pow::commitment(&request.encrypted_payload));

        // Validate the request
        if let Err(e) = self.validate_request(&request) {
            warn!(
                commitment = %commitment,
                error = %e,
                "Rejected encrypted transaction"
            );
            return Ok(EncryptedTransactionResponse {
                accepted: false,
                commitment,
                error: Some(e.to_string()),
            });
        }

        debug!(
            commitment = %commitment,
            payload_size = request.encrypted_payload.len(),
            "Validated encrypted transaction"
        );

        // Forward to sequencer via P2P if configured
        if let Some(forwarder) = &self.p2p_forwarder {
            info!(
                commitment = %commitment,
                "Forwarding encrypted transaction to sequencer via P2P"
            );

            match forwarder.forward(&request).await {
                Ok(response) => {
                    if response.accepted {
                        info!(
                            commitment = %commitment,
                            "Sequencer accepted P2P-forwarded transaction"
                        );
                    } else {
                        warn!(
                            commitment = %commitment,
                            error = ?response.error,
                            "Sequencer rejected P2P-forwarded transaction"
                        );
                    }
                    Ok(response)
                }
                Err(e) => {
                    warn!(
                        commitment = %commitment,
                        error = %e,
                        "Failed to forward via P2P"
                    );
                    Ok(EncryptedTransactionResponse {
                        accepted: false,
                        commitment,
                        error: Some(format!("P2P forwarding failed: {e}")),
                    })
                }
            }
        }
        // Forward to sequencer via HTTP if configured
        else if let Some(forwarder) = &self.http_forwarder {
            info!(
                commitment = %commitment,
                sequencer_url = %forwarder.sequencer_url,
                "Forwarding encrypted transaction to sequencer via HTTP"
            );

            match forwarder.forward(&request).await {
                Ok(response) => {
                    if response.accepted {
                        info!(
                            commitment = %commitment,
                            "Sequencer accepted HTTP-forwarded transaction"
                        );
                    } else {
                        warn!(
                            commitment = %commitment,
                            error = ?response.error,
                            "Sequencer rejected HTTP-forwarded transaction"
                        );
                    }
                    Ok(response)
                }
                Err(e) => {
                    warn!(
                        commitment = %commitment,
                        error = %e,
                        "Failed to forward via HTTP"
                    );
                    Ok(EncryptedTransactionResponse {
                        accepted: false,
                        commitment,
                        error: Some(format!("HTTP forwarding failed: {e}")),
                    })
                }
            }
        } else {
            // No forwarder - just accept (legacy mode)
            debug!(commitment = %commitment, "Accepted (legacy mode, no forwarding)");
            Ok(EncryptedTransactionResponse {
                accepted: true,
                commitment,
                error: None,
            })
        }
    }

    async fn get_relay_parameters(&self) -> RpcResult<RelayParameters> {
        let params = self.config.get();
        Ok((*params).clone())
    }
}

/// Implementation for sequencer mode with Eth API for pool submission.
#[async_trait]
impl<Eth> EncryptedRelayApiServer for EncryptedRelayApiImpl<Eth>
where
    Eth: EthTransactions + Send + Sync + 'static,
{
    async fn send_encrypted_transaction(
        &self,
        request: EncryptedTransactionRequest,
    ) -> RpcResult<EncryptedTransactionResponse> {
        // Compute commitment hash for tracking
        let commitment = B256::from(pow::commitment(&request.encrypted_payload));

        // Validate the request
        if let Err(e) = self.validate_request(&request) {
            warn!(
                commitment = %commitment,
                error = %e,
                "Rejected encrypted transaction"
            );
            return Ok(EncryptedTransactionResponse {
                accepted: false,
                commitment,
                error: Some(e.to_string()),
            });
        }

        debug!(
            commitment = %commitment,
            payload_size = request.encrypted_payload.len(),
            "Validated encrypted transaction"
        );

        // Sequencer mode with decryption - decrypt and submit to pool
        match self.decrypt_and_submit(&request, commitment).await {
            Ok((decrypted, tx_hash)) => {
                info!(
                    commitment = %commitment,
                    decrypted_size = decrypted.len(),
                    tx_hash = ?tx_hash,
                    "Successfully decrypted and submitted transaction"
                );
                Ok(EncryptedTransactionResponse {
                    accepted: true,
                    commitment,
                    error: None,
                })
            }
            Err(e) => {
                warn!(
                    commitment = %commitment,
                    error = %e,
                    "Failed to decrypt transaction"
                );
                Ok(EncryptedTransactionResponse {
                    accepted: false,
                    commitment,
                    error: Some(format!("decryption failed: {e}")),
                })
            }
        }
    }

    async fn get_relay_parameters(&self) -> RpcResult<RelayParameters> {
        let params = self.config.get();
        Ok((*params).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use base_reth_relay::{crypto, pow};

    fn test_config() -> Arc<RelayConfigCache> {
        let (_, pubkey) = crypto::generate_keypair();
        let attestation = [0u8; 32];
        Arc::new(RelayConfigCache::with_defaults(pubkey, attestation))
    }

    fn test_config_with_key() -> (Arc<RelayConfigCache>, [u8; 32]) {
        let (secret, pubkey) = crypto::generate_keypair();
        let attestation = [0u8; 32];
        (Arc::new(RelayConfigCache::with_defaults(pubkey, attestation)), secret)
    }

    #[tokio::test]
    async fn test_send_encrypted_transaction_valid() {
        let config = test_config();
        let api = EncryptedRelayApiImpl::new(config.clone());

        let params = config.get();
        let encryption_pubkey: [u8; 32] = params.encryption_pubkey.as_ref().try_into().unwrap();

        // Create a valid encrypted payload
        let tx_data = b"test transaction data";
        let encrypted = crypto::encrypt(&encryption_pubkey, tx_data).unwrap();
        let nonce = pow::compute(&encrypted, params.pow_difficulty);

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from(encrypted),
            pow_nonce: nonce,
            encryption_pubkey: params.encryption_pubkey.clone(),
        };

        let response = api.send_encrypted_transaction(request).await.unwrap();
        assert!(response.accepted);
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn test_send_encrypted_transaction_invalid_pow() {
        let config = test_config();
        let api = EncryptedRelayApiImpl::new(config.clone());

        let params = config.get();

        // Create payload with invalid PoW nonce
        let encrypted = vec![0u8; MIN_ENCRYPTED_PAYLOAD_SIZE];

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from(encrypted),
            pow_nonce: 0, // Invalid nonce
            encryption_pubkey: params.encryption_pubkey.clone(),
        };

        let response = api.send_encrypted_transaction(request).await.unwrap();
        assert!(!response.accepted);
        assert!(response.error.is_some());
        assert!(response.error.unwrap().contains("proof-of-work"));
    }

    #[tokio::test]
    async fn test_send_encrypted_transaction_invalid_key() {
        let config = test_config();
        let api = EncryptedRelayApiImpl::new(config);

        // Use a different (invalid) encryption key
        let (_, wrong_pubkey) = crypto::generate_keypair();
        let encrypted = vec![0u8; MIN_ENCRYPTED_PAYLOAD_SIZE];

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from(encrypted),
            pow_nonce: 0,
            encryption_pubkey: Bytes::copy_from_slice(&wrong_pubkey),
        };

        let response = api.send_encrypted_transaction(request).await.unwrap();
        assert!(!response.accepted);
        assert!(response.error.is_some());
        assert!(response.error.unwrap().contains("encryption key"));
    }

    #[tokio::test]
    async fn test_send_encrypted_transaction_payload_too_small() {
        let config = test_config();
        let api = EncryptedRelayApiImpl::new(config.clone());

        let params = config.get();

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from_static(&[1, 2, 3]), // Too small
            pow_nonce: 0,
            encryption_pubkey: params.encryption_pubkey.clone(),
        };

        let response = api.send_encrypted_transaction(request).await.unwrap();
        assert!(!response.accepted);
        assert!(response.error.is_some());
        assert!(response.error.unwrap().contains("too small"));
    }

    #[tokio::test]
    async fn test_get_relay_parameters() {
        let config = test_config();
        let api = EncryptedRelayApiImpl::new(config.clone());

        let params = api.get_relay_parameters().await.unwrap();

        assert_eq!(params.pow_difficulty, base_reth_relay::DEFAULT_DIFFICULTY);
        assert_eq!(params.encryption_pubkey.len(), 32);
        assert_eq!(params.attestation_pubkey.len(), 32);
    }
}
