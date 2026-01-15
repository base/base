//! Test client for connecting to a live node-reth instance.

use std::sync::Arc;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{Filter, Log};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, OpTransactionRequest};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::{
    harness::{WebSocketSubscription, decode_ws_message},
    types::{Bundle, MeterBundleResponse, OpBlock},
};

/// Client for interacting with a live node-reth instance.
#[derive(Debug)]
pub struct TestClient {
    /// HTTP provider for standard RPC calls.
    provider: RootProvider<Optimism>,
    /// Raw RPC client for custom methods.
    rpc_client: RpcClient,
    /// HTTP RPC URL for the node being tested.
    pub rpc_url: String,
    /// Flashblocks WebSocket URL (sequencer/builder endpoint).
    pub flashblocks_ws_url: String,
    /// Transaction signer (optional - only if private key provided).
    signer: Option<PrivateKeySigner>,
    /// Recipient address for ETH transfers in tests.
    recipient: Option<Address>,
    /// Simulator contract address for state root timing tests.
    simulator: Option<Address>,
    /// Chain ID for signing transactions.
    chain_id: u64,
    /// Tracked nonce for the signer address (to avoid race conditions with remote sequencers).
    next_nonce: Arc<Mutex<Option<u64>>>,
}

impl TestClient {
    /// Create a new test client connected to the given endpoints.
    ///
    /// # Arguments
    /// * `rpc_url` - HTTP RPC endpoint URL
    /// * `flashblocks_ws_url` - Flashblocks WebSocket URL
    /// * `private_key` - Optional hex-encoded private key for signing transactions
    /// * `recipient` - Optional recipient address for ETH transfers in tests
    /// * `simulator` - Optional Simulator contract address for state root timing tests
    ///
    /// # Errors
    /// Returns an error if recipient equals the signer address.
    pub async fn new(
        rpc_url: &str,
        flashblocks_ws_url: &str,
        private_key: Option<&str>,
        recipient: Option<Address>,
        simulator: Option<Address>,
    ) -> Result<Self> {
        let url: Url = rpc_url.parse().wrap_err("Invalid RPC URL")?;

        // Build provider using RpcClient directly (similar to node-reth's test harness)
        let http_client = RpcClient::builder().http(url.clone());
        let provider = RootProvider::<Optimism>::new(http_client);

        let rpc_client = RpcClient::new_http(url);

        // Fetch chain ID from the node
        let chain_id = provider.get_chain_id().await.wrap_err("Failed to get chain ID from RPC")?;
        tracing::info!(chain_id, "Connected to chain");

        // Parse private key if provided
        let signer = if let Some(key) = private_key { Some(parse_private_key(key)?) } else { None };

        // Validate recipient != sender
        if let (Some(s), Some(r)) = (&signer, recipient)
            && s.address() == r
        {
            return Err(eyre::eyre!(
                "Recipient address cannot be the same as the signer address ({})",
                r
            ));
        }

        Ok(Self {
            provider,
            rpc_client,
            rpc_url: rpc_url.to_string(),
            flashblocks_ws_url: flashblocks_ws_url.to_string(),
            signer,
            recipient,
            simulator,
            chain_id,
            next_nonce: Arc::new(Mutex::new(None)),
        })
    }

    /// Check if we have a signer configured.
    pub const fn has_signer(&self) -> bool {
        self.signer.is_some()
    }

    /// Get the signer's address, if configured.
    pub fn signer_address(&self) -> Option<Address> {
        self.signer.as_ref().map(|s| s.address())
    }

    /// Get the recipient address for ETH transfers, if configured.
    pub const fn recipient(&self) -> Option<Address> {
        self.recipient
    }

    /// Get the Simulator contract address, if configured.
    pub const fn simulator(&self) -> Option<Address> {
        self.simulator
    }

    /// Get the chain ID.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Get the next nonce for the signer, tracking it locally to avoid race conditions.
    ///
    /// On first call, fetches from pending state. Subsequent calls increment locally.
    /// This prevents "nonce too low" errors when testing against remote sequencers
    /// where the pending state may not immediately reflect forwarded transactions.
    ///
    /// Call `reset_nonce()` if a transaction fails and you need to re-fetch.
    pub async fn get_next_nonce(&self) -> Result<u64> {
        let mut guard = self.next_nonce.lock().await;
        let nonce = match *guard {
            Some(n) => n,
            None => {
                let from =
                    self.signer_address().ok_or_else(|| eyre::eyre!("No signer configured"))?;
                self.get_transaction_count(from, BlockNumberOrTag::Pending).await?
            }
        };
        *guard = Some(nonce + 1);
        Ok(nonce)
    }

    /// Peek at the current nonce without incrementing.
    ///
    /// Use this for transactions that will only be simulated (e.g., metering) and not
    /// actually sent to the network. For transactions that will be sent, use
    /// `get_next_nonce()` instead.
    pub async fn peek_nonce(&self) -> Result<u64> {
        let guard = self.next_nonce.lock().await;
        match *guard {
            Some(n) => Ok(n),
            None => {
                let from =
                    self.signer_address().ok_or_else(|| eyre::eyre!("No signer configured"))?;
                self.get_transaction_count(from, BlockNumberOrTag::Pending).await
            }
        }
    }

    /// Reset the tracked nonce (e.g., after a failed transaction).
    pub async fn reset_nonce(&self) {
        let mut guard = self.next_nonce.lock().await;
        *guard = None;
    }

    /// Get the underlying provider.
    pub const fn provider(&self) -> &RootProvider<Optimism> {
        &self.provider
    }

    /// Get block by number.
    pub async fn get_block_by_number(&self, block: BlockNumberOrTag) -> Result<Option<OpBlock>> {
        self.provider.get_block_by_number(block).await.wrap_err("Failed to get block by number")
    }

    /// Get balance at address.
    pub async fn get_balance(&self, address: Address, block: BlockNumberOrTag) -> Result<U256> {
        self.provider
            .get_balance(address)
            .block_id(block.into())
            .await
            .wrap_err("Failed to get balance")
    }

    /// Get transaction count (nonce) for address.
    pub async fn get_transaction_count(
        &self,
        address: Address,
        block: BlockNumberOrTag,
    ) -> Result<u64> {
        self.provider
            .get_transaction_count(address)
            .block_id(block.into())
            .await
            .wrap_err("Failed to get transaction count")
    }

    /// Execute eth_call.
    pub async fn eth_call(
        &self,
        tx: &OpTransactionRequest,
        block: BlockNumberOrTag,
    ) -> Result<Bytes> {
        self.provider.call(tx.clone()).block(block.into()).await.wrap_err("eth_call failed")
    }

    /// Estimate gas for transaction.
    pub async fn estimate_gas(
        &self,
        tx: &OpTransactionRequest,
        block: BlockNumberOrTag,
    ) -> Result<u64> {
        self.provider
            .estimate_gas(tx.clone())
            .block(block.into())
            .await
            .wrap_err("Failed to estimate gas")
    }

    /// Get transaction by hash.
    pub async fn get_transaction_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<op_alloy_rpc_types::Transaction>> {
        self.provider
            .get_transaction_by_hash(hash)
            .await
            .wrap_err("Failed to get transaction by hash")
    }

    /// Get transaction receipt.
    pub async fn get_transaction_receipt(
        &self,
        hash: B256,
    ) -> Result<Option<OpTransactionReceipt>> {
        self.provider
            .get_transaction_receipt(hash)
            .await
            .wrap_err("Failed to get transaction receipt")
    }

    /// Get logs matching filter.
    pub async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>> {
        self.provider.get_logs(filter).await.wrap_err("Failed to get logs")
    }

    /// Sign a transaction request and return the raw transaction bytes and hash.
    pub fn sign_transaction(&self, tx_request: OpTransactionRequest) -> Result<(Bytes, B256)> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("No signer configured - set PRIVATE_KEY"))?;

        // Build the typed transaction
        let tx = tx_request
            .build_typed_tx()
            .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

        // Sign it
        let signature = signer.sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);

        // Encode and return
        let tx_bytes = Bytes::from(signed_tx.encoded_2718());
        let tx_hash = *signed_tx.hash();

        Ok((tx_bytes, tx_hash))
    }

    /// Create and sign a simple ETH transfer transaction.
    ///
    /// If `nonce` is `None`, uses the tracked nonce from `get_next_nonce()` to avoid
    /// race conditions when testing against remote sequencers.
    pub async fn create_transfer(
        &self,
        to: Address,
        value: U256,
        nonce: Option<u64>,
    ) -> Result<(Bytes, B256)> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("No signer configured - set PRIVATE_KEY"))?;

        let from = signer.address();

        // Get nonce - use tracked nonce if not explicitly provided
        let nonce = match nonce {
            Some(n) => n,
            None => self.get_next_nonce().await?,
        };

        let mut tx_request = OpTransactionRequest::default()
            .from(from)
            .to(to)
            .value(value)
            .nonce(nonce)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000) // 1 gwei
            .max_priority_fee_per_gas(1_000_000);
        tx_request.set_chain_id(self.chain_id);

        self.sign_transaction(tx_request)
    }

    /// Send a signed raw transaction.
    pub async fn send_raw_transaction(&self, tx: Bytes) -> Result<B256> {
        self.provider
            .send_raw_transaction(&tx)
            .await
            .wrap_err("Failed to send raw transaction")
            .map(|pending| *pending.tx_hash())
    }

    /// Send raw transaction and wait for receipt (sync mode).
    pub async fn send_raw_transaction_sync(
        &self,
        tx: Bytes,
        timeout_ms: Option<u64>,
    ) -> Result<OpTransactionReceipt> {
        self.rpc_client
            .request::<_, OpTransactionReceipt>("eth_sendRawTransactionSync", (tx, timeout_ms))
            .await
            .wrap_err("eth_sendRawTransactionSync failed")
    }

    /// Send ETH and wait for confirmation.
    pub async fn send_eth_and_wait(
        &self,
        to: Address,
        value: U256,
    ) -> Result<OpTransactionReceipt> {
        let (tx_bytes, tx_hash) = self.create_transfer(to, value, None).await?;
        tracing::debug!(?tx_hash, "Sending transaction");

        // Use sync mode to wait for receipt
        self.send_raw_transaction_sync(tx_bytes, Some(6_000)).await
    }

    /// Call `base_meterBundle` RPC method.
    pub async fn meter_bundle(&self, bundle: Bundle) -> Result<MeterBundleResponse> {
        self.rpc_client
            .request::<_, MeterBundleResponse>("base_meterBundle", (bundle,))
            .await
            .wrap_err("base_meterBundle failed")
    }

    /// Call `base_meteredPriorityFeePerGas` RPC method.
    pub async fn metered_priority_fee(
        &self,
        bundle: Bundle,
    ) -> Result<crate::types::MeteredPriorityFeeResponse> {
        self.rpc_client
            .request::<_, crate::types::MeteredPriorityFeeResponse>(
                "base_meteredPriorityFeePerGas",
                (bundle,),
            )
            .await
            .wrap_err("base_meteredPriorityFeePerGas failed")
    }

    /// Connect to WebSocket and subscribe to a topic.
    pub async fn ws_subscribe(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<WebSocketSubscription> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::connect_async;

        let (mut ws_stream, _) = connect_async(&self.flashblocks_ws_url)
            .await
            .wrap_err("Failed to connect to WebSocket")?;

        let subscribe_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_subscribe",
            "params": [method, params]
        });

        ws_stream
            .send(Message::Text(subscribe_msg.to_string().into()))
            .await
            .wrap_err("Failed to send subscribe message")?;

        // Wait for subscription confirmation
        let response = ws_stream
            .next()
            .await
            .ok_or_else(|| eyre::eyre!("WebSocket closed unexpectedly"))?
            .wrap_err("Failed to receive subscription response")?;

        let json_str = decode_ws_message(&response)?;
        let sub_response: serde_json::Value =
            serde_json::from_str(&json_str).wrap_err("Failed to parse subscription response")?;

        let subscription_id = sub_response["result"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("No subscription ID in response"))?
            .to_string();

        Ok(WebSocketSubscription { stream: ws_stream, subscription_id })
    }
}

/// Parse a private key from hex string (with or without 0x prefix).
fn parse_private_key(key: &str) -> Result<PrivateKeySigner> {
    let key = key.strip_prefix("0x").unwrap_or(key);
    let key_bytes = hex::decode(key).wrap_err("Invalid hex in private key")?;

    if key_bytes.len() != 32 {
        return Err(eyre::eyre!("Private key must be 32 bytes, got {}", key_bytes.len()));
    }

    let key_fixed: FixedBytes<32> = FixedBytes::from_slice(&key_bytes);
    PrivateKeySigner::from_bytes(&key_fixed).wrap_err("Invalid private key")
}
