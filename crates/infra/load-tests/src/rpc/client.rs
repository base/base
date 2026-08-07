use std::{fmt::Display, time::Duration};

use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, Bytes, TxHash};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{ChainIdFiller, FillProvider, JoinFill, WalletFiller},
};
use base_common_network::Base;
use futures::future::join_all;
use tokio::sync::Semaphore;
use tracing::{instrument, warn};
use url::Url;

use crate::utils::{BaselineError, Result};

/// Default timeout for load-test RPC HTTP requests.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Read-only provider type for querying Base nodes.
pub type QueryProvider = RootProvider<Base>;

/// Provider type with wallet signing capability for sending transactions.
///
/// Uses Ethereum network type because `send_transaction` works identically
/// for both Ethereum and Base networks.
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

/// Factory for Alloy RPC providers used by the load tester.
#[derive(Debug, Clone, Copy)]
pub struct RpcProviders;

impl RpcProviders {
    /// Creates a read-only query provider with the load-test RPC timeout.
    pub fn query(url: Url) -> Result<QueryProvider> {
        let client = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .build()
            .map_err(|e| BaselineError::Rpc(format!("failed to build RPC HTTP client: {e}")))?;
        Ok(ProviderBuilder::<Identity, Identity, Base>::default().connect_reqwest(client, url))
    }
}

/// Extension trait for reading the latest base fee from a query provider.
pub trait BaseFeeExt {
    /// Returns the `baseFeePerGas` of the latest block.
    ///
    /// Base fee is the value that determines whether a `maxFeePerGas` is
    /// includable, so fee estimation reads it directly rather than relying on
    /// `eth_gasPrice` (which can lag or smooth on some clients).
    fn get_base_fee(&self) -> impl std::future::Future<Output = Result<u128>> + Send;
}

impl BaseFeeExt for QueryProvider {
    async fn get_base_fee(&self) -> Result<u128> {
        let block = self
            .get_block_by_number(alloy_rpc_types::BlockNumberOrTag::Latest)
            .hashes()
            .await
            .rpc("get latest block for base fee")?
            .ok_or_else(|| BaselineError::Rpc("latest block not found".to_string()))?;
        block
            .header
            .base_fee_per_gas
            .map(u128::from)
            .ok_or_else(|| BaselineError::Rpc("latest block missing base fee".to_string()))
    }
}

/// Extension trait for converting Alloy RPC results into load-test errors.
pub trait RpcResultExt<T> {
    /// Converts an RPC result into the load-test result type with context.
    fn rpc(self, context: &'static str) -> Result<T>;
}

impl<T, E> RpcResultExt<T> for std::result::Result<T, E>
where
    E: Display,
{
    fn rpc(self, context: &'static str) -> Result<T> {
        self.map_err(|e| BaselineError::Rpc(format!("{context}: {e}")))
    }
}

/// Client for txpool administration RPC methods used by load tests.
#[derive(Clone)]
pub struct TxpoolAdminClient {
    provider: QueryProvider,
    url: Url,
}

impl TxpoolAdminClient {
    /// Creates a txpool administration client for the given endpoint.
    pub fn new(url: Url) -> Result<Self> {
        let provider = RpcProviders::query(url.clone())?;
        Ok(Self { provider, url })
    }

    /// Drops all pending transactions from the given sender address.
    #[instrument(skip(self), fields(address = %address))]
    pub async fn drop_sender_transactions(&self, address: Address) -> Result<Vec<TxHash>> {
        self.provider
            .client()
            .request("admin_dropSenderTransactions", (address,))
            .await
            .rpc("drop sender transactions")
    }

    /// Returns pending and queued transaction nonces for a sender, respectively.
    pub async fn sender_transaction_nonces(
        &self,
        address: Address,
    ) -> Result<(Vec<u64>, Vec<u64>)> {
        let content: serde_json::Value = self
            .provider
            .client()
            .request("txpool_contentFrom", (address,))
            .await
            .rpc("get sender txpool content")?;
        let parse_section = |section: &str| -> Result<Vec<u64>> {
            let mut nonces = Vec::new();
            let Some(transactions) = content.get(section).and_then(serde_json::Value::as_object)
            else {
                return Ok(nonces);
            };
            for nonce in transactions.keys() {
                let parsed = nonce
                    .strip_prefix("0x")
                    .map_or_else(|| nonce.parse(), |hex| u64::from_str_radix(hex, 16))
                    .map_err(|e| {
                        BaselineError::Rpc(format!(
                            "invalid nonce {nonce} in sender txpool content: {e}"
                        ))
                    })?;
                nonces.push(parsed);
            }
            Ok(nonces)
        };
        Ok((parse_section("pending")?, parse_section("queued")?))
    }

    /// Returns the RPC endpoint URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }
}

impl std::fmt::Debug for TxpoolAdminClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxpoolAdminClient").field("url", &self.url).finish_non_exhaustive()
    }
}

/// Maximum number of JSON-RPC calls per batch HTTP request.
///
/// Public RPC endpoints (e.g. Sepolia) often reject or return non-JSON error
/// responses for very large batches. Keeping batches small avoids rate-limit
/// and gateway errors.
pub const MAX_BATCH_RPC_SIZE: usize = 100;

/// Client for JSON-RPC batch requests.
///
/// Wraps `reqwest::Client` to send multiple JSON-RPC calls in a single HTTP
/// request, dramatically reducing per-request overhead (TLS, TCP, HTTP framing)
/// when submitting many transactions simultaneously.
#[derive(Clone, Debug)]
pub struct BatchRpcClient {
    client: reqwest::Client,
    url: Url,
    batch_size: usize,
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
            .timeout(RPC_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(256)
            .tcp_nodelay(true)
            .build()
            .expect("failed to build reqwest client");
        Self { client, url, batch_size: MAX_BATCH_RPC_SIZE }
    }

    /// Sets the maximum number of JSON-RPC calls in each HTTP request.
    pub const fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = if batch_size == 0 { 1 } else { batch_size };
        self
    }

    /// Sends multiple pre-signed raw transactions via JSON-RPC batch requests.
    /// Returns one [`BatchSendResult`] per input, preserving order.
    ///
    /// Large requests are automatically split into configured sub-batches and
    /// sent concurrently. When supplied, `request_limiter` bounds concurrency
    /// across all batches and sender workers.
    ///
    /// `request_limiter`, when set, bounds the number of these sub-batch HTTP
    /// requests that may be outstanding concurrently across all callers
    /// sharing the semaphore, independent of how many sender workers exist or
    /// how many transactions are unconfirmed.
    ///
    /// Each element in `raw_txs` must be the EIP-2718 encoded signed
    /// transaction bytes (as produced by `Encodable2718::encoded_2718`).
    pub async fn send_raw_transactions(
        &self,
        raw_txs: &[Bytes],
        request_limiter: Option<&Semaphore>,
    ) -> Result<Vec<BatchSendResult>> {
        if raw_txs.is_empty() {
            return Ok(Vec::new());
        }

        let chunk_requests = raw_txs.chunks(self.batch_size).map(|chunk| async move {
            let _permit = match request_limiter {
                Some(limiter) => Some(limiter.acquire().await.expect("semaphore never closed")),
                None => None,
            };
            self.send_raw_chunk(chunk).await
        });
        let chunk_results = join_all(chunk_requests).await;

        let mut all_results = Vec::with_capacity(raw_txs.len());
        for result in chunk_results {
            all_results.extend(result?);
        }

        Ok(all_results)
    }

    async fn send_raw_chunk(&self, chunk: &[Bytes]) -> Result<Vec<BatchSendResult>> {
        let batch: Vec<serde_json::Value> = chunk
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

        let response =
            self.client.post(self.url.as_str()).json(&batch).send().await.map_err(|error| {
                let error_kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection"
                } else if error.is_request() {
                    "request"
                } else {
                    "transport"
                };
                BaselineError::Rpc(format!("batch send request failed ({error_kind})"))
            })?;

        let status = response.status();
        let body_text = response.text().await.map_err(|error| {
            let error_kind = if error.is_timeout() { "timeout" } else { "transport" };
            BaselineError::Rpc(format!("failed to read batch send response body ({error_kind})"))
        })?;

        if !status.is_success() {
            let preview = truncate_for_log(&body_text);
            return Err(BaselineError::Rpc(format!(
                "batch send request returned HTTP {status}: {preview}"
            )));
        }

        let body: Vec<serde_json::Value> = serde_json::from_str(&body_text).map_err(|e| {
            let preview = truncate_for_log(&body_text);
            BaselineError::Rpc(format!(
                "batch send response is not a JSON array: {e} (body: {preview})"
            ))
        })?;

        let mut results: Vec<BatchSendResult> =
            (0..chunk.len()).map(|_| BatchSendResult::Error("missing response".into())).collect();

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
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                results[id] = BatchSendResult::Error(msg.to_string());
            }
        }

        Ok(results)
    }
}

fn truncate_for_log(s: &str) -> &str {
    let max = 256;
    if s.len() <= max { s } else { &s[..s.floor_char_boundary(max)] }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use alloy_primitives::Bytes;
    use url::Url;

    use super::BatchRpcClient;

    #[tokio::test]
    async fn batch_transport_error_omits_endpoint_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read test endpoint");
        drop(listener);
        let url =
            Url::parse(&format!("http://user:secret@{address}")).expect("valid credentialed URL");
        let client = BatchRpcClient::new(url);

        let error = client
            .send_raw_transactions(&[Bytes::from(vec![1])], None)
            .await
            .expect_err("closed endpoint should reject request")
            .to_string();

        assert!(error.contains("batch send request failed"));
        assert!(!error.contains("user"));
        assert!(!error.contains("secret"));
        assert!(!error.contains(&address.to_string()));
    }

    #[test]
    fn batch_size_is_configurable() {
        let url = Url::parse("http://localhost:8545").unwrap();

        assert_eq!(BatchRpcClient::new(url).with_batch_size(25).batch_size, 25);
    }
}
