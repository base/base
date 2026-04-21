use std::{future::Future, sync::Arc, time::Duration};

use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{ChainIdFiller, FillProvider, JoinFill, WalletFiller},
};
use alloy_rpc_types::BlockNumberOrTag;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionReceipt;
use parking_lot::RwLock;
use tracing::{debug, instrument, warn};
use url::Url;

use crate::utils::{BaselineError, Result};

type BlockTimestampCache = Arc<RwLock<std::collections::HashMap<u64, u64>>>;

/// Provider trait for fetching transaction receipts and block data.
///
/// This trait abstracts the RPC calls needed by the confirmer, enabling
/// mock implementations for testing.
pub trait ReceiptProvider: Send + Sync {
    /// Fetches transaction hashes from the pending block.
    fn get_pending_block_tx_hashes(&self) -> impl Future<Output = Result<Vec<TxHash>>> + Send;

    /// Fetches the transaction receipt for a given hash.
    fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> impl Future<Output = Result<Option<BaseTransactionReceipt>>> + Send;

    /// Fetches the block timestamp (unix seconds) for a given block number.
    fn get_block_timestamp(
        &self,
        block_number: u64,
    ) -> impl Future<Output = Result<Option<u64>>> + Send;

    /// Fetches all transaction receipts for a given block number.
    ///
    /// Returns `None` if the block does not exist. Significantly more efficient
    /// than individual receipt lookups when confirming many transactions, since
    /// one RPC call retrieves all receipts in the block at once.
    fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> impl Future<Output = Result<Option<Vec<BaseTransactionReceipt>>>> + Send;

    /// Fetches the latest block number.
    fn get_latest_block_number(&self) -> impl Future<Output = Result<u64>> + Send;
}

/// Provider type with wallet signing capability for sending transactions.
///
/// Uses Ethereum network type because `send_transaction` works identically
/// for both Ethereum and Base networks. Only `RpcClient` uses the Base network
/// type since it needs `BaseTransactionReceipt` for receipt handling.
pub type WalletProvider = FillProvider<
    JoinFill<JoinFill<Identity, ChainIdFiller>, WalletFiller<EthereumWallet>>,
    RootProvider<Ethereum>,
    Ethereum,
>;

/// Creates a wallet provider for the given RPC URL and wallet.
pub fn create_wallet_provider(rpc_url: Url, wallet: EthereumWallet) -> WalletProvider {
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .filler(ChainIdFiller::default())
        .wallet(wallet)
        .connect_http(rpc_url)
}

/// RPC client for read-only interactions with Base nodes.
#[derive(Clone)]
pub struct RpcClient {
    provider: RootProvider<Base>,
    url: Url,
    block_timestamp_cache: BlockTimestampCache,
}

impl RpcClient {
    /// Creates a new RPC client.
    pub fn new(url: Url) -> Self {
        let provider = RootProvider::<Base>::new_http(url.clone());
        Self { provider, url, block_timestamp_cache: Arc::new(RwLock::new(Default::default())) }
    }

    /// Returns the RPC endpoint URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches the chain ID from the RPC endpoint.
    #[instrument(skip(self), fields(url = %self.url))]
    pub async fn chain_id(&self) -> Result<u64> {
        self.provider.get_chain_id().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the balance of an address at the latest block.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_balance(&self, address: Address) -> Result<U256> {
        self.provider.get_balance(address).await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the balance of an address including pending transactions.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_pending_balance(&self, address: Address) -> Result<U256> {
        self.provider
            .get_balance(address)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the nonce (transaction count) for an address.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn get_nonce(&self, address: Address) -> Result<u64> {
        self.provider
            .get_transaction_count(address)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the transaction receipt for a given hash.
    #[instrument(skip(self), fields(tx_hash = %tx_hash))]
    pub async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<BaseTransactionReceipt>> {
        self.provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the current gas price.
    #[instrument(skip(self))]
    pub async fn get_gas_price(&self) -> Result<u128> {
        self.provider.get_gas_price().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches transaction hashes from the pending block via `eth_getBlockByNumber("pending")`.
    #[instrument(skip(self))]
    pub async fn get_pending_block_tx_hashes(&self) -> Result<Vec<TxHash>> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .hashes()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        Ok(block.map(|b| b.transactions.hashes().collect()).unwrap_or_default())
    }

    /// Fetches the block timestamp (unix seconds) for a given block number, with caching.
    #[instrument(skip(self))]
    pub async fn get_block_timestamp(&self, block_number: u64) -> Result<Option<u64>> {
        if let Some(&ts) = self.block_timestamp_cache.read().get(&block_number) {
            return Ok(Some(ts));
        }

        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .hashes()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        let Some(block) = block else {
            return Ok(None);
        };

        let timestamp = block.header.timestamp;
        self.block_timestamp_cache.write().insert(block_number, timestamp);

        Ok(Some(timestamp))
    }

    /// Fetches all transaction receipts for a block via `eth_getBlockReceipts`.
    #[instrument(skip(self))]
    pub async fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> Result<Option<Vec<BaseTransactionReceipt>>> {
        self.provider
            .get_block_receipts(alloy_eips::BlockId::Number(BlockNumberOrTag::Number(
                block_number,
            )))
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))
    }

    /// Fetches the latest block number.
    #[instrument(skip(self))]
    pub async fn get_latest_block_number(&self) -> Result<u64> {
        self.provider.get_block_number().await.map_err(|e| BaselineError::Rpc(e.to_string()))
    }
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").field("url", &self.url).finish_non_exhaustive()
    }
}

impl ReceiptProvider for RpcClient {
    async fn get_pending_block_tx_hashes(&self) -> Result<Vec<TxHash>> {
        self.get_pending_block_tx_hashes().await
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<BaseTransactionReceipt>> {
        self.get_transaction_receipt(tx_hash).await
    }

    async fn get_block_timestamp(&self, block_number: u64) -> Result<Option<u64>> {
        self.get_block_timestamp(block_number).await
    }

    async fn get_block_receipts(
        &self,
        block_number: u64,
    ) -> Result<Option<Vec<BaseTransactionReceipt>>> {
        self.get_block_receipts(block_number).await
    }

    async fn get_latest_block_number(&self) -> Result<u64> {
        self.get_latest_block_number().await
    }
}

/// Client for JSON-RPC batch requests.
///
/// Wraps `reqwest::Client` to send multiple JSON-RPC calls in a single HTTP
/// request, dramatically reducing per-request overhead (TLS, TCP, HTTP framing)
/// when submitting many transactions simultaneously.
#[derive(Clone, Debug)]
pub struct BatchRpcClient {
    client: reqwest::Client,
    url: Url,
}

/// Result of a single request within a JSON-RPC batch response.
#[derive(Debug)]
pub enum BatchSendResult {
    /// Transaction was accepted; contains the transaction hash.
    Success(TxHash),
    /// Transaction was rejected with the given error message.
    Error(String),
}

impl BatchRpcClient {
    /// Creates a new batch RPC client targeting the given endpoint.
    pub fn new(url: Url) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self { client, url }
    }

    /// Fetches transaction receipts for multiple hashes in a single JSON-RPC
    /// batch request. Returns one `Option<BaseTransactionReceipt>` per input,
    /// preserving order. `None` means the receipt was not found (tx pending
    /// or unknown).
    pub async fn batch_get_transaction_receipts(
        &self,
        tx_hashes: &[TxHash],
    ) -> Result<Vec<Option<BaseTransactionReceipt>>> {
        if tx_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let batch: Vec<serde_json::Value> = tx_hashes
            .iter()
            .enumerate()
            .map(|(i, hash)| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "method": "eth_getTransactionReceipt",
                    "params": [hash]
                })
            })
            .collect();

        let response = self
            .client
            .post(self.url.as_str())
            .json(&batch)
            .send()
            .await
            .map_err(|e| {
                BaselineError::Rpc(format!("batch receipt request failed: {e}"))
            })?;

        let body: Vec<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| {
                BaselineError::Rpc(format!("batch receipt response parse failed: {e}"))
            })?;

        let mut results: Vec<Option<BaseTransactionReceipt>> = vec![None; tx_hashes.len()];

        for item in body {
            let id = item["id"].as_u64().unwrap_or(u64::MAX) as usize;
            if id >= results.len() {
                continue;
            }

            if let Some(result) = item.get("result") {
                if !result.is_null() {
                    match serde_json::from_value::<BaseTransactionReceipt>(result.clone()) {
                        Ok(receipt) => results[id] = Some(receipt),
                        Err(e) => {
                            debug!(id, error = %e, "failed to parse receipt in batch response");
                        }
                    }
                }
            }
        }

        debug!(count = tx_hashes.len(), "batch receipt fetch complete");
        Ok(results)
    }

    /// Sends multiple pre-signed raw transactions in a single JSON-RPC batch
    /// request. Returns one [`BatchSendResult`] per input, preserving order.
    ///
    /// Each element in `raw_txs` must be the EIP-2718 encoded signed
    /// transaction bytes (as produced by `Encodable2718::encoded_2718`).
    pub async fn send_raw_transactions(
        &self,
        raw_txs: &[Bytes],
    ) -> Result<Vec<BatchSendResult>> {
        if raw_txs.is_empty() {
            return Ok(Vec::new());
        }

        let batch: Vec<serde_json::Value> = raw_txs
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "method": "eth_sendRawTransaction",
                    "params": [raw]
                })
            })
            .collect();

        let response = self
            .client
            .post(self.url.as_str())
            .json(&batch)
            .send()
            .await
            .map_err(|e| BaselineError::Rpc(format!("batch send request failed: {e}")))?;

        let body: Vec<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| BaselineError::Rpc(format!("batch send response parse failed: {e}")))?;

        let mut results: Vec<BatchSendResult> =
            (0..raw_txs.len()).map(|_| BatchSendResult::Error("missing response".into())).collect();

        for item in body {
            let id = item["id"].as_u64().unwrap_or(u64::MAX) as usize;
            if id >= results.len() {
                warn!(id, "batch response contained out-of-range id");
                continue;
            }

            if let Some(result) = item.get("result").and_then(|v| v.as_str()) {
                match result.parse::<TxHash>() {
                    Ok(hash) => results[id] = BatchSendResult::Success(hash),
                    Err(e) => {
                        results[id] = BatchSendResult::Error(format!("invalid tx hash: {e}"));
                    }
                }
            } else if let Some(error) = item.get("error") {
                let msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                results[id] = BatchSendResult::Error(msg.to_string());
            }
        }

        debug!(count = raw_txs.len(), "batch send complete");
        Ok(results)
    }
}
