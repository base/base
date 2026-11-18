use std::{
    borrow::Cow,
    fmt::Display,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::{Address, Bytes, Signature, TxHash};
use alloy_provider::{
    Identity, Provider, ProviderBuilder, RootProvider,
    fillers::{ChainIdFiller, FillProvider, JoinFill, WalletFiller},
};
use base_common_network::Base;
use base_execution_txpool::{ValidityAuthorizationError, ValidityPredicate};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
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

/// JSON-RPC standard error code for an unrecognized method (method not found).
///
/// Used to detect when a submission endpoint does not serve
/// `base_sendRawTransactionValidity`, so the load tester can fail loudly rather
/// than silently degrade to plain submission.
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;

/// The `eth_sendRawTransaction` JSON-RPC method name.
const ETH_SEND_RAW_TRANSACTION: &str = "eth_sendRawTransaction";

/// The `base_sendRawTransactionValidity` JSON-RPC method name.
const BASE_SEND_RAW_TRANSACTION_VALIDITY: &str = "base_sendRawTransactionValidity";

/// A single transaction to submit within a batch, borrowed from the signed transaction.
///
/// An empty `validity` list is submitted via `eth_sendRawTransaction`; a
/// non-empty list is submitted via `base_sendRawTransactionValidity` carrying
/// the predicates. Mixed batches are supported: the method is selected
/// per element, and responses are correlated by JSON-RPC `id`.
#[derive(Debug, Clone, Copy)]
pub struct SubmitItem<'a> {
    /// EIP-2718 encoded signed transaction bytes.
    pub raw: &'a Bytes,
    /// State predicates transported alongside the transaction. Empty for a
    /// plain `eth_sendRawTransaction` submission.
    pub validity: &'a [ValidityPredicate],
    /// Optional sender authorization for the validity sidecar.
    pub validity_signature: Option<Signature>,
}

impl<'a> SubmitItem<'a> {
    /// Creates a plain submission item with no validity predicates.
    pub const fn plain(raw: &'a Bytes) -> Self {
        Self { raw, validity: &[], validity_signature: None }
    }

    /// Creates a submission item carrying validity predicates.
    pub const fn with_validity(raw: &'a Bytes, validity: &'a [ValidityPredicate]) -> Self {
        Self { raw, validity, validity_signature: None }
    }

    /// Returns true when this item submits via `base_sendRawTransactionValidity`.
    pub const fn is_validity(&self) -> bool {
        !self.validity.is_empty()
    }
}

/// One JSON-RPC request in a submission batch, serialized straight from borrowed transaction
/// bytes and predicates without building an intermediate JSON value tree.
#[derive(Debug, Serialize)]
pub struct SubmitRequest<'a> {
    /// JSON-RPC protocol version, always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Position of the item in its chunk, echoed back by the node.
    pub id: usize,
    /// `eth_sendRawTransaction` or `base_sendRawTransactionValidity`.
    pub method: &'static str,
    /// Positional parameters for `method`.
    pub params: SubmitParams<'a>,
}

/// Positional parameters of a [`SubmitRequest`].
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SubmitParams<'a> {
    /// `eth_sendRawTransaction` parameters: the raw transaction.
    Plain((&'a Bytes,)),
    /// `base_sendRawTransactionValidity` parameters: the raw transaction and its sidecar.
    Validity((&'a Bytes, ValiditySidecar<'a>)),
}

/// Borrowed validity sidecar, wire-compatible with `TransactionValidity`.
#[derive(Debug, Serialize)]
pub struct ValiditySidecar<'a> {
    /// State predicates the transaction is valid under.
    pub validity: &'a [ValidityPredicate],
    /// Optional sender authorization, omitted from the JSON when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validity_signature: Option<Signature>,
}

impl<'a> From<(usize, &SubmitItem<'a>)> for SubmitRequest<'a> {
    fn from((id, item): (usize, &SubmitItem<'a>)) -> Self {
        let (method, params) = if item.is_validity() {
            (
                BASE_SEND_RAW_TRANSACTION_VALIDITY,
                SubmitParams::Validity((
                    item.raw,
                    ValiditySidecar {
                        validity: item.validity,
                        validity_signature: item.validity_signature,
                    },
                )),
            )
        } else {
            (ETH_SEND_RAW_TRANSACTION, SubmitParams::Plain((item.raw,)))
        };
        Self { jsonrpc: "2.0", id, method, params }
    }
}

/// One JSON-RPC response in a submission batch, borrowing strings from the response body.
#[derive(Debug, Deserialize)]
pub struct SubmitReply<'a> {
    /// Id of the request this reply answers.
    #[serde(default)]
    pub id: Option<u64>,
    /// Transaction hash, when the node accepted the transaction.
    #[serde(default, borrow)]
    pub result: Option<ReplyStr<'a>>,
    /// Error object, when the node rejected the transaction.
    #[serde(default, borrow)]
    pub error: Option<SubmitErrorReply<'a>>,
}

/// JSON-RPC error object of a [`SubmitReply`].
#[derive(Debug, Deserialize)]
pub struct SubmitErrorReply<'a> {
    /// JSON-RPC error code.
    #[serde(default)]
    pub code: Option<i64>,
    /// Human-readable error message.
    #[serde(default, borrow)]
    pub message: Option<ReplyStr<'a>>,
    /// Arbitrary JSON, left unparsed; only a string value is a stable rejection reason.
    #[serde(default, borrow)]
    pub data: Option<&'a RawValue>,
}

/// String field of a [`SubmitReply`], borrowed from the response body unless it contains escapes.
///
/// Serde only borrows a `Cow<str>` field declared directly, not one nested in `Option`, so
/// optional reply strings wrap it in this newtype.
#[derive(Debug, Deserialize)]
pub struct ReplyStr<'a>(#[serde(borrow)] pub Cow<'a, str>);

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
    validity_signatures_required: Arc<AtomicBool>,
}

/// Result of a single request within a JSON-RPC batch response.
#[derive(Debug)]
pub enum BatchSendResult {
    /// Transaction was accepted; contains the transaction hash.
    Success(TxHash),
    /// Transaction was rejected with the given error.
    Error(BatchSendError),
}

/// Error returned for a single request within a JSON-RPC batch response.
///
/// Carries the JSON-RPC error `code` alongside the message so callers can
/// distinguish structured conditions (e.g. [`JSON_RPC_METHOD_NOT_FOUND`]) from
/// message-based classification. Client-side failures (missing response,
/// unparseable hash) use `code = None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSendError {
    /// JSON-RPC error code, when the failure came from a server error object.
    pub code: Option<i64>,
    /// Human-readable error message.
    pub message: String,
    /// Stable rejection reason supplied by signature-aware RPC nodes.
    pub reason: Option<String>,
}

impl BatchSendError {
    /// Creates an error carrying a JSON-RPC error code.
    pub fn with_code(code: i64, message: impl Into<String>) -> Self {
        Self { code: Some(code), message: message.into(), reason: None }
    }

    /// Creates a client-side error with no JSON-RPC code.
    pub fn client(message: impl Into<String>) -> Self {
        Self { code: None, message: message.into(), reason: None }
    }

    /// Returns true only for an explicit signature-required ingress rejection.
    pub fn is_signature_required(&self) -> bool {
        self.code == Some(-32602)
            && (self.reason.as_deref()
                == Some(ValidityAuthorizationError::MissingSignature.as_label())
                || self.message == ValidityAuthorizationError::MissingSignature.to_string())
    }

    /// Returns true when this is a JSON-RPC method-not-found error.
    pub fn is_method_not_found(&self) -> bool {
        self.code == Some(JSON_RPC_METHOD_NOT_FOUND)
    }
}

impl Display for BatchSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "{} (code {code})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl BatchRpcClient {
    /// Creates a new batch RPC client targeting the given endpoint.
    pub fn new(url: Url) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(256)
            .tcp_nodelay(true)
            .build()
            .map_err(|e| {
                BaselineError::Rpc(format!("failed to build batch RPC HTTP client: {e}"))
            })?;
        Ok(Self {
            client,
            url,
            batch_size: MAX_BATCH_RPC_SIZE,
            validity_signatures_required: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Remembers that this endpoint explicitly requires signed validity sidecars.
    /// Clones share this one-way state; a signed request is never downgraded.
    pub fn require_validity_signatures(&self) {
        self.validity_signatures_required.store(true, Ordering::Relaxed);
    }

    /// Returns whether an explicit rejection has required signing for this endpoint.
    pub fn validity_signatures_required(&self) -> bool {
        self.validity_signatures_required.load(Ordering::Relaxed)
    }

    /// Sets the maximum number of JSON-RPC calls in each HTTP request.
    pub const fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = if batch_size == 0 { 1 } else { batch_size };
        self
    }

    /// Sends multiple pre-signed transactions via JSON-RPC batch requests.
    /// Returns one [`BatchSendResult`] per input, preserving order.
    ///
    /// Each [`SubmitItem`] selects its own method: items without validity
    /// predicates use `eth_sendRawTransaction`, items carrying predicates use
    /// `base_sendRawTransactionValidity`. A single HTTP batch may mix both;
    /// responses are correlated back to inputs by JSON-RPC `id`.
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
    /// Each item's `raw` field must be the EIP-2718 encoded signed transaction
    /// bytes (as produced by `Encodable2718::encoded_2718`).
    pub async fn send_raw_transactions(
        &self,
        items: &[SubmitItem<'_>],
        request_limiter: Option<&Semaphore>,
    ) -> Result<Vec<BatchSendResult>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let chunk_requests = items.chunks(self.batch_size).map(|chunk| async move {
            let _permit = match request_limiter {
                Some(limiter) => Some(limiter.acquire().await.map_err(|e| {
                    BaselineError::Rpc(format!("request limiter semaphore closed: {e}"))
                })?),
                None => None,
            };
            self.send_raw_chunk(chunk).await
        });
        let chunk_results = join_all(chunk_requests).await;

        let mut all_results = Vec::with_capacity(items.len());
        for result in chunk_results {
            all_results.extend(result?);
        }

        Ok(all_results)
    }

    /// Builds the JSON-RPC batch array for a chunk of submission items,
    /// selecting the method per element and assigning sequential ids.
    fn build_batch_body<'a>(chunk: &[SubmitItem<'a>]) -> Vec<SubmitRequest<'a>> {
        chunk.iter().enumerate().map(SubmitRequest::from).collect()
    }

    /// Correlates a JSON-RPC batch response array back to one
    /// [`BatchSendResult`] per input, keyed by response `id`. Inputs with no
    /// matching or well-formed response remain a client-side "missing response" error,
    /// so one malformed element never discards the rest of the chunk.
    fn parse_batch_body(len: usize, body: Vec<&RawValue>) -> Vec<BatchSendResult> {
        let mut results: Vec<BatchSendResult> = (0..len)
            .map(|_| BatchSendResult::Error(BatchSendError::client("missing response")))
            .collect();

        for element in body {
            let reply = match serde_json::from_str::<SubmitReply<'_>>(element.get()) {
                Ok(reply) => reply,
                Err(error) => {
                    warn!(error = %error, "skipping malformed batch response element");
                    continue;
                }
            };
            let Some(id) = reply.id.and_then(|id| usize::try_from(id).ok()).filter(|id| *id < len)
            else {
                warn!(id = ?reply.id, "batch response contained out-of-range id");
                continue;
            };

            if let Some(result) = reply.result {
                results[id] = match result.0.parse::<TxHash>() {
                    Ok(hash) => BatchSendResult::Success(hash),
                    Err(e) => BatchSendResult::Error(BatchSendError::client(format!(
                        "invalid tx hash: {e}"
                    ))),
                };
            } else if let Some(error) = reply.error {
                let message = error.message.map_or(Cow::Borrowed("unknown error"), |m| m.0);
                let mut err = error.code.map_or_else(
                    || BatchSendError::client(message.as_ref()),
                    |code| BatchSendError::with_code(code, message.as_ref()),
                );
                err.reason =
                    error.data.and_then(|data| serde_json::from_str::<String>(data.get()).ok());
                results[id] = BatchSendResult::Error(err);
            }
        }

        results
    }

    async fn send_raw_chunk(&self, chunk: &[SubmitItem<'_>]) -> Result<Vec<BatchSendResult>> {
        let batch = Self::build_batch_body(chunk);

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
        let body_bytes = response.bytes().await.map_err(|error| {
            let error_kind = if error.is_timeout() { "timeout" } else { "transport" };
            BaselineError::Rpc(format!("failed to read batch send response body ({error_kind})"))
        })?;

        if !status.is_success() {
            let body_text = String::from_utf8_lossy(&body_bytes);
            let preview = truncate_for_log(&body_text);
            return Err(BaselineError::Rpc(format!(
                "batch send request returned HTTP {status}: {preview}"
            )));
        }

        let body: Vec<&RawValue> = serde_json::from_slice(&body_bytes).map_err(|e| {
            let body_text = String::from_utf8_lossy(&body_bytes);
            let preview = truncate_for_log(&body_text);
            BaselineError::Rpc(format!(
                "batch send response is not a JSON-RPC array: {e} (body: {preview})"
            ))
        })?;

        Ok(Self::parse_batch_body(chunk.len(), body))
    }
}

fn truncate_for_log(s: &str) -> &str {
    let max = 256;
    if s.len() <= max { s } else { &s[..s.floor_char_boundary(max)] }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use alloy_primitives::{U256, address};
    use base_execution_txpool::ValidityOperator;

    use super::*;

    fn balance_predicate() -> ValidityPredicate {
        ValidityPredicate::Balance {
            address: address!("00000000000000000000000000000000000000aa"),
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1u64),
        }
    }

    #[tokio::test]
    async fn batch_transport_error_omits_endpoint_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read test endpoint");
        drop(listener);
        let url =
            Url::parse(&format!("http://user:secret@{address}")).expect("valid credentialed URL");
        let client = BatchRpcClient::new(url).expect("build batch client");

        let raw = Bytes::from(vec![1]);
        let error = client
            .send_raw_transactions(&[SubmitItem::plain(&raw)], None)
            .await
            .expect_err("closed endpoint should reject request")
            .to_string();

        assert!(error.contains("batch send request failed"));
        assert!(!error.contains("user"));
        assert!(!error.contains("secret"));
        assert!(!error.contains(&address.to_string()));
    }

    #[test]
    fn build_batch_body_mixes_methods_and_ids() {
        let (plain, validity_raw) = (Bytes::from_static(&[0xaa]), Bytes::from_static(&[0xbb]));
        let predicates = [balance_predicate()];
        let items =
            [SubmitItem::plain(&plain), SubmitItem::with_validity(&validity_raw, &predicates)];

        let body = serde_json::to_value(BatchRpcClient::build_batch_body(&items)).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(body[0]["jsonrpc"], "2.0");

        // Plain element: eth_sendRawTransaction with a positional raw param.
        assert_eq!(body[0]["id"], 0);
        assert_eq!(body[0]["method"], ETH_SEND_RAW_TRANSACTION);
        assert_eq!(body[0]["params"][0], "0xaa");

        // Validity element: base_sendRawTransactionValidity with positional
        // params leading with the raw tx hex string, followed by { validity }.
        assert_eq!(body[1]["id"], 1);
        assert_eq!(body[1]["method"], BASE_SEND_RAW_TRANSACTION_VALIDITY);
        assert_eq!(body[1]["params"][0], "0xbb");
        assert_eq!(body[1]["params"][1]["validity"][0]["type"], "balance");
        assert_eq!(body[1]["params"][1]["validity"][0]["params"]["op"], ">=");
        assert!(body[1]["params"][1].get("validity_signature").is_none());
    }

    #[test]
    fn parse_batch_body_correlates_by_id() {
        let hash = "0x".to_string() + &"11".repeat(32);
        let body = serde_json::json!([
            { "jsonrpc": "2.0", "id": 1, "result": hash },
            {
                "jsonrpc": "2.0",
                "id": 0,
                "error": { "code": -32000, "message": "nonce too low", "data": { "nested": 1 } }
            },
            { "jsonrpc": "2.0", "id": 2, "error": { "code": -32602, "message": "m", "data": "why" } },
            { "jsonrpc": "2.0", "id": 3, "result": 7 },
            { "jsonrpc": "2.0", "id": 9, "result": hash },
            { "jsonrpc": "2.0", "id": null, "error": { "message": "parse error" } },
        ])
        .to_string();

        let results = BatchRpcClient::parse_batch_body(
            4,
            serde_json::from_str(&body).expect("valid JSON array"),
        );
        match &results[0] {
            BatchSendResult::Error(e) => {
                assert_eq!(e.code, Some(-32000));
                assert_eq!(e.message, "nonce too low");
                assert_eq!(e.reason, None, "non-string data is not a reason");
            }
            other => panic!("expected error, got {other:?}"),
        }
        assert!(matches!(results[1], BatchSendResult::Success(_)));
        match &results[2] {
            BatchSendResult::Error(e) => assert_eq!(e.reason.as_deref(), Some("why")),
            other => panic!("expected error, got {other:?}"),
        }
        match &results[3] {
            BatchSendResult::Error(e) => assert_eq!(e.message, "missing response"),
            other => panic!("malformed element must only fail its own slot, got {other:?}"),
        }
    }

    #[test]
    fn submit_reply_borrows_unescaped_strings() {
        let body = r#"{"id":0,"result":"0xabc","error":{"message":"m","data":"d"}}"#;
        let reply: SubmitReply<'_> = serde_json::from_str(body).unwrap();
        assert!(matches!(reply.result.unwrap().0, Cow::Borrowed("0xabc")));
        assert!(matches!(reply.error.unwrap().message.unwrap().0, Cow::Borrowed("m")));

        let escaped: SubmitReply<'_> =
            serde_json::from_str(r#"{"error":{"message":"a\"b"}}"#).unwrap();
        assert_eq!(escaped.error.unwrap().message.unwrap().0, "a\"b");
    }

    #[test]
    fn parse_batch_body_marks_missing_responses() {
        let results = BatchRpcClient::parse_batch_body(2, Vec::new());
        assert_eq!(results.len(), 2);
        for result in &results {
            match result {
                BatchSendResult::Error(e) => {
                    assert_eq!(e.code, None);
                    assert_eq!(e.message, "missing response");
                }
                other => panic!("expected error, got {other:?}"),
            }
        }
    }

    #[test]
    fn batch_send_error_display_and_method_not_found() {
        let error = BatchSendError::with_code(JSON_RPC_METHOD_NOT_FOUND, "nope");
        assert!(error.is_method_not_found());
        assert_eq!(error.to_string(), "nope (code -32601)");
        assert!(!BatchSendError::client("boom").is_method_not_found());
    }
}
