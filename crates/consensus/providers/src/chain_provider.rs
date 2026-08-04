//! Providers that use alloy provider types on the backend.

use std::{boxed::Box, num::NonZeroUsize, vec::Vec};

use alloy_consensus::{Header, Receipt, TxEnvelope};
use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_transport::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineError, PipelineErrorKind, ResetError};
use base_protocol::BlockInfo;
use lru::LruCache;

use crate::{L1RpcProvider, Metrics};

/// The result of a background read-ahead task: the resolved block hash with its
/// header and receipts, or `None` if any fetch failed.
type ReadAheadResult = Option<(B256, Header, Vec<Receipt>)>;

/// The [`AlloyChainProvider`] is a concrete implementation of the [`ChainProvider`] trait, providing
/// data over Ethereum JSON-RPC using an alloy provider as the backend.
///
/// [`ChainProvider::hint_block`] spawns a best-effort background task that
/// fetches the hinted block's header and receipts. The task hands its result
/// back through a [`tokio::task::JoinHandle`]; the provider absorbs a finished
/// result into its caches on the next `header_by_hash` or `receipts_by_hash`
/// call, so the hinted block's fetch becomes a cache hit instead of synchronous
/// RPC on the critical path. An unfinished read-ahead is never awaited.
#[derive(Debug)]
pub struct AlloyChainProvider {
    /// The inner Ethereum JSON-RPC provider.
    pub inner: RootProvider,
    /// Whether to trust the RPC without verification.
    pub trust_rpc: bool,
    /// The in-flight read-ahead task spawned by [`ChainProvider::hint_block`],
    /// if any.
    pending_read_ahead: Option<tokio::task::JoinHandle<ReadAheadResult>>,
    /// `header_by_hash` LRU cache.
    header_by_hash_cache: LruCache<B256, Header>,
    /// `receipts_by_hash_cache` LRU cache.
    receipts_by_hash_cache: LruCache<B256, Vec<Receipt>>,
    /// `block_info_and_transactions_by_hash` LRU cache.
    block_info_and_transactions_by_hash_cache: LruCache<B256, (BlockInfo, Vec<TxEnvelope>)>,
}

impl Clone for AlloyChainProvider {
    /// Clones the provider and its caches. The pending read-ahead task is not
    /// cloned ([`tokio::task::JoinHandle`] is not [`Clone`]); the clone starts
    /// with no read-ahead in flight.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            trust_rpc: self.trust_rpc,
            pending_read_ahead: None,
            header_by_hash_cache: self.header_by_hash_cache.clone(),
            receipts_by_hash_cache: self.receipts_by_hash_cache.clone(),
            block_info_and_transactions_by_hash_cache: self
                .block_info_and_transactions_by_hash_cache
                .clone(),
        }
    }
}

impl AlloyChainProvider {
    /// Creates a new [`AlloyChainProvider`] with the given alloy provider.
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new(inner: RootProvider, cache_size: usize) -> Self {
        Self::new_with_trust(inner, cache_size, true)
    }

    /// Creates a new [`AlloyChainProvider`] with the given alloy provider and trust setting.
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new_with_trust(inner: RootProvider, cache_size: usize, trust_rpc: bool) -> Self {
        Self {
            inner,
            trust_rpc,
            pending_read_ahead: None,
            header_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            receipts_by_hash_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
            block_info_and_transactions_by_hash_cache: LruCache::new(
                NonZeroUsize::new(cache_size).unwrap(),
            ),
        }
    }

    /// Creates a new [`AlloyChainProvider`] from the provided [`url::Url`].
    ///
    /// The underlying HTTP provider uses the shared [`crate::L1_RPC_TIMEOUT`] deadline.
    pub fn new_http(url: url::Url, cache_size: usize) -> Self {
        let inner = L1RpcProvider::new_http(url);
        Self::new(inner, cache_size)
    }

    /// Returns the latest L2 block number.
    pub async fn latest_block_number(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        Metrics::chain_rpc_calls("block_number").increment(1);

        let result = base_metrics::time!(Metrics::request_duration("block_number"), {
            self.inner.get_block_number().await
        });

        if result.is_err() {
            Metrics::chain_rpc_errors("block_number").increment(1);
        }

        result
    }

    /// Returns the chain ID.
    pub async fn chain_id(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        self.inner.get_chain_id().await
    }

    /// Verifies that a header's hash matches the expected hash when `trust_rpc` is false.
    fn verify_header_hash(
        &self,
        header: &Header,
        expected_hash: B256,
    ) -> Result<(), AlloyChainProviderError> {
        if self.trust_rpc {
            return Ok(());
        }

        let actual_hash = header.hash_slow();
        if actual_hash != expected_hash {
            return Err(AlloyChainProviderError::Transport(RpcError::Transport(
                TransportErrorKind::Custom(
                    format!(
                        "Header hash mismatch: expected {expected_hash:?}, got {actual_hash:?}"
                    )
                    .into(),
                ),
            )));
        }

        Ok(())
    }

    /// Absorbs the result of a finished read-ahead task into the caches.
    ///
    /// If a read-ahead is still in flight it is left alone — the critical path
    /// never awaits an unfinished read-ahead. Failed read-aheads are discarded;
    /// the caller falls back to a synchronous fetch on the next cache miss.
    async fn absorb_finished_read_ahead(&mut self) {
        let Some(handle) = self.pending_read_ahead.take() else {
            return;
        };

        if !handle.is_finished() {
            self.pending_read_ahead = Some(handle);
            return;
        }

        if let Ok(Some((hash, header, receipts))) = handle.await {
            self.header_by_hash_cache.put(hash, header);
            Metrics::cache_entries("header_by_hash").increment(1);
            self.receipts_by_hash_cache.put(hash, receipts);
            Metrics::cache_entries("receipts_by_hash").increment(1);
        }
    }
}

/// An error for the [`AlloyChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum AlloyChainProviderError {
    /// Transport error
    #[error(transparent)]
    Transport(#[from] RpcError<TransportErrorKind>),
    /// Block not found.
    #[error("Block not found: {0}")]
    BlockNotFound(BlockId),
    /// Failed to convert RPC receipts into consensus receipts.
    #[error("Failed to convert RPC receipts into consensus receipts: {0}")]
    ReceiptsConversion(B256),
}

impl From<AlloyChainProviderError> for PipelineErrorKind {
    fn from(e: AlloyChainProviderError) -> Self {
        match e {
            AlloyChainProviderError::Transport(e) => {
                Self::Temporary(PipelineError::Provider(format!("Transport error: {e}")))
            }
            AlloyChainProviderError::BlockNotFound(id) => {
                // A hash-based lookup returning not-found means the block was reorged out —
                // retrying will never succeed, so reset.
                // A number-based lookup returning not-found means the next L1 block hasn't
                // been produced yet — this is transient, so Temporary.
                match id {
                    BlockId::Hash(_) => ResetError::BlockNotFound(id).reset(),
                    BlockId::Number(_) => Self::Temporary(PipelineError::Provider(format!(
                        "L1 Block not found: {id}"
                    ))),
                }
            }
            AlloyChainProviderError::ReceiptsConversion(_) => {
                Self::Temporary(PipelineError::Provider(
                    "Failed to convert RPC receipts into consensus receipts".to_string(),
                ))
            }
        }
    }
}

#[async_trait]
impl ChainProvider for AlloyChainProvider {
    type Error = AlloyChainProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        self.absorb_finished_read_ahead().await;

        if let Some(header) = self.header_by_hash_cache.get(&hash) {
            Metrics::chain_cache_hits("header_by_hash").increment(1);
            return Ok(header.clone());
        }

        Metrics::chain_cache_misses("header_by_hash").increment(1);

        Metrics::chain_rpc_calls("header_by_hash").increment(1);

        let block = base_metrics::time!(Metrics::request_duration("header_by_hash"), {
            self.inner.get_block_by_hash(hash).await
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("header_by_hash").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))?;
        let header = block.header.into_consensus();

        // Verify the header hash matches what we requested
        self.verify_header_hash(&header, hash)?;

        self.header_by_hash_cache.put(hash, header.clone());

        Metrics::cache_entries("header_by_hash").increment(1);

        Ok(header)
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        Metrics::chain_rpc_calls("block_by_number").increment(1);

        let block = base_metrics::time!(Metrics::request_duration("block_by_number"), {
            self.inner.get_block_by_number(number.into()).await
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("block_by_number").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(number.into()))?;
        let header = block.header.into_consensus();

        let block_info = BlockInfo {
            hash: header.hash_slow(),
            number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        };
        Ok(block_info)
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        self.absorb_finished_read_ahead().await;

        if let Some(receipts) = self.receipts_by_hash_cache.get(&hash) {
            Metrics::chain_cache_hits("receipts_by_hash").increment(1);
            return Ok(receipts.clone());
        }

        Metrics::chain_cache_misses("receipts_by_hash").increment(1);

        Metrics::chain_rpc_calls("receipts_by_hash").increment(1);

        let receipts = base_metrics::time!(Metrics::request_duration("receipts_by_hash"), {
            self.inner.get_block_receipts(hash.into()).await
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("receipts_by_hash").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))?;
        let consensus_receipts = receipts
            .into_iter()
            .map(|r| r.inner.into_primitives_receipt().as_receipt().cloned())
            .collect::<Option<Vec<_>>>()
            .ok_or(AlloyChainProviderError::ReceiptsConversion(hash))?;

        self.receipts_by_hash_cache.put(hash, consensus_receipts.clone());

        Metrics::cache_entries("receipts_by_hash").increment(1);

        Ok(consensus_receipts)
    }

    /// Spawns a best-effort background task that fetches the header and
    /// receipts of the block at `number` and hands them back through
    /// [`Self::pending_read_ahead`]. The hint is skipped if a read-ahead is
    /// already pending.
    ///
    /// The task uses the (cheaply cloned, `Arc`-backed) RPC client directly:
    /// the hinted block is new and cannot be cached yet, and the block-by-number
    /// response already contains the header, so resolving number → hash → header
    /// through the provider methods would re-fetch the same header. The hash is
    /// computed from the fetched header, so no `trust_rpc` verification applies.
    ///
    /// Failures are ignored: the block may not exist yet, and the caller falls
    /// back to a synchronous fetch on the next cache miss.
    ///
    /// Must be called from within a tokio runtime.
    fn hint_block(&mut self, number: u64) {
        if self.pending_read_ahead.is_some() {
            return;
        }

        let inner = self.inner.clone();
        self.pending_read_ahead = Some(tokio::spawn(async move {
            Metrics::chain_rpc_calls("block_by_number").increment(1);
            let block = base_metrics::time!(Metrics::request_duration("block_by_number"), {
                inner.get_block_by_number(number.into()).await
            })
            .inspect_err(|err| {
                Metrics::chain_rpc_errors("block_by_number").increment(1);
                tracing::debug!(
                    target: "chain_provider",
                    error = %err,
                    number,
                    "Read-ahead block fetch failed"
                );
            })
            .ok()??;
            let header = block.header.into_consensus();
            let hash = header.hash_slow();

            Metrics::chain_rpc_calls("receipts_by_hash").increment(1);
            let receipts = base_metrics::time!(Metrics::request_duration("receipts_by_hash"), {
                inner.get_block_receipts(hash.into()).await
            })
            .inspect_err(|err| {
                Metrics::chain_rpc_errors("receipts_by_hash").increment(1);
                tracing::debug!(
                    target: "chain_provider",
                    error = %err,
                    number,
                    "Read-ahead receipts fetch failed"
                );
            })
            .ok()??;
            let receipts = receipts
                .into_iter()
                .map(|r| r.inner.into_primitives_receipt().as_receipt().cloned())
                .collect::<Option<Vec<_>>>()?;

            Some((hash, header, receipts))
        }));
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        if let Some(block_info_and_txs) = self.block_info_and_transactions_by_hash_cache.get(&hash)
        {
            Metrics::chain_cache_hits("block_info_and_tx").increment(1);
            return Ok(block_info_and_txs.clone());
        }

        Metrics::chain_cache_misses("block_info_and_tx").increment(1);

        Metrics::chain_rpc_calls("block_by_hash").increment(1);

        let block = base_metrics::time!(Metrics::request_duration("block_by_hash"), {
            self.inner.get_block_by_hash(hash).full().await
        })
        .inspect_err(|_e| {
            Metrics::chain_rpc_errors("block_by_hash").increment(1);
        })?
        .ok_or(AlloyChainProviderError::BlockNotFound(hash.into()))?
        .into_consensus()
        .map_transactions(|t| t.inner.into_inner());

        // Verify the block hash matches what we requested
        self.verify_header_hash(&block.header, hash)?;

        let block_info = BlockInfo {
            hash, // Use the already verified hash instead of recomputing
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        };

        self.block_info_and_transactions_by_hash_cache
            .put(hash, (block_info, block.body.transactions.clone()));

        Metrics::cache_entries("block_info_and_tx").increment(1);

        Ok((block_info, block.body.transactions))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::B256;
    use alloy_rpc_types_eth::Block;
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use serde_json::{Value, json};

    use super::*;

    type RecordedCalls = Arc<Mutex<Vec<(String, Value)>>>;

    fn block_json(number: u64) -> Value {
        json!({
            "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "parentHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0x3333333333333333333333333333333333333333333333333333333333333333",
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": format!("0x{number:x}"),
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x1",
            "extraData": "0x",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "transactions": [],
            "uncles": [],
            "withdrawals": [],
            "blobGasUsed": "0x0",
            "excessBlobGas": "0x0"
        })
    }

    fn json_rpc_response(req: &HttpMockRequest, result: Value) -> String {
        let id = serde_json::from_slice::<Value>(&req.body_vec())
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(Value::Null);
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    /// Mounts a catch-all JSON-RPC dispatcher that serves block 42 (by hash
    /// `h1`), block 43 (by number and by `next_hash`), and empty receipts for
    /// any hash, recording every `(method, params)` pair in `calls`.
    async fn mount_dispatcher(
        server: &MockServer,
        h1: B256,
        next_hash: B256,
        calls: RecordedCalls,
    ) {
        server
            .mock_async(move |when, then| {
                when.method(POST).path("/");
                then.respond_with(move |req| {
                    let body: Value = serde_json::from_slice(&req.body_vec()).unwrap();
                    let method = body["method"].as_str().unwrap_or_default().to_string();
                    let params = body["params"].clone();
                    calls.lock().unwrap().push((method.clone(), params.clone()));
                    let result = match method.as_str() {
                        "eth_getBlockReceipts" => json!([]),
                        "eth_getBlockByNumber" if params[0] == json!("0x2b") => block_json(43),
                        "eth_getBlockByHash" if params[0] == json!(format!("{h1:?}")) => {
                            block_json(42)
                        }
                        "eth_getBlockByHash" if params[0] == json!(format!("{next_hash:?}")) => {
                            block_json(43)
                        }
                        _ => Value::Null,
                    };
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, result))
                        .build()
                });
            })
            .await;
    }

    fn l1_provider(server: &MockServer) -> AlloyChainProvider {
        AlloyChainProvider::new(RootProvider::new_http(server.url("/").parse().unwrap()), 16)
    }

    fn count(calls: &RecordedCalls, method: &str) -> usize {
        calls.lock().unwrap().iter().filter(|(m, _)| m == method).count()
    }

    /// Waits for the provider's pending read-ahead task to finish, without
    /// absorbing it.
    async fn wait_for_read_ahead(provider: &AlloyChainProvider) {
        let handle = provider.pending_read_ahead.as_ref().expect("no read-ahead pending");
        for _ in 0..200 {
            if handle.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("read-ahead task never finished");
    }

    #[tokio::test]
    async fn test_receipts_by_hash_does_not_prefetch() {
        let server = MockServer::start_async().await;
        let calls: RecordedCalls = Arc::default();
        let h1 = B256::repeat_byte(0xaa);
        mount_dispatcher(&server, h1, B256::ZERO, Arc::clone(&calls)).await;

        let mut provider = l1_provider(&server);
        provider.header_by_hash(h1).await.unwrap();
        provider.receipts_by_hash(h1).await.unwrap();

        assert!(provider.pending_read_ahead.is_none(), "fetches must not spawn read-ahead");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            count(&calls, "eth_getBlockByNumber"),
            0,
            "fetches must not trigger background block-by-number lookups"
        );
    }

    #[tokio::test]
    async fn test_hint_block_warms_caches() {
        let server = MockServer::start_async().await;
        let calls: RecordedCalls = Arc::default();
        let h1 = B256::repeat_byte(0xaa);
        // `block_info_by_number` derives the hash from the fetched header, so
        // compute the hash the read-ahead task will resolve for block 43.
        let next_block: Block = serde_json::from_value(block_json(43)).unwrap();
        let next_hash = next_block.header.inner.hash_slow();
        mount_dispatcher(&server, h1, next_hash, Arc::clone(&calls)).await;

        let mut provider = l1_provider(&server);
        provider.hint_block(43);
        wait_for_read_ahead(&provider).await;

        // Exactly two RPCs: block by number (which includes the header) and
        // receipts. No header re-fetch by hash, no cascade to block 44.
        assert_eq!(count(&calls, "eth_getBlockByNumber"), 1);
        assert_eq!(count(&calls, "eth_getBlockByHash"), 0);
        assert_eq!(count(&calls, "eth_getBlockReceipts"), 1);

        // The next fetch absorbs the finished read-ahead and serves the hinted
        // block from cache: no new RPCs.
        provider.header_by_hash(next_hash).await.unwrap();
        provider.receipts_by_hash(next_hash).await.unwrap();
        assert!(provider.pending_read_ahead.is_none(), "finished read-ahead must be absorbed");
        assert_eq!(count(&calls, "eth_getBlockByHash"), 0, "header must come from the cache");
        assert_eq!(count(&calls, "eth_getBlockReceipts"), 1, "receipts must come from the cache");
    }

    #[tokio::test]
    async fn test_hint_block_skipped_while_pending() {
        let server = MockServer::start_async().await;
        let calls: RecordedCalls = Arc::default();
        let next_block: Block = serde_json::from_value(block_json(43)).unwrap();
        let next_hash = next_block.header.inner.hash_slow();
        mount_dispatcher(&server, B256::repeat_byte(0xaa), next_hash, Arc::clone(&calls)).await;

        let mut provider = l1_provider(&server);
        provider.hint_block(43);
        provider.hint_block(43);
        wait_for_read_ahead(&provider).await;

        assert_eq!(
            count(&calls, "eth_getBlockByNumber"),
            1,
            "a hint while one is pending must be dropped"
        );
    }

    #[test]
    fn test_from_alloy_chain_provider_error() {
        // Transport errors are transient — retry makes sense.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::Transport(alloy_transport::RpcError::Transport(
                alloy_transport::TransportErrorKind::Custom("timeout".into()),
            ))
            .into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // ReceiptsConversion is a transient decode failure.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::ReceiptsConversion(Default::default()).into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // Hash-based BlockNotFound: the block was reorged out. Retrying will never succeed
        // — the pipeline must reset. Without this, the safe head stalls on L1 reorgs.
        let kind: PipelineErrorKind =
            AlloyChainProviderError::BlockNotFound(B256::default().into()).into();
        assert!(
            matches!(kind, PipelineErrorKind::Reset(_)),
            "hash-based BlockNotFound must map to Reset (block reorged out)"
        );

        // Number-based BlockNotFound: the next L1 block hasn't been mined yet. This is
        // transient — the pipeline must wait, not reset.
        let kind: PipelineErrorKind = AlloyChainProviderError::BlockNotFound(0u64.into()).into();
        assert!(
            matches!(kind, PipelineErrorKind::Temporary(_)),
            "number-based BlockNotFound must stay Temporary (block not yet produced)"
        );
    }
}
