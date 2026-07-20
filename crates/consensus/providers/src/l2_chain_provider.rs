//! Providers that use alloy provider types on the backend.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use alloy_eips::BlockId;
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::JwtSecret;
use alloy_transport::{RpcError, TransportErrorKind};
use alloy_transport_http::{
    AuthLayer, Http, HyperClient,
    hyper_util::{client::legacy::Client, rt::TokioExecutor},
};
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_network::Base;
use base_consensus_derive::{L2ChainProvider, PipelineError, PipelineErrorKind, ResetError};
use base_protocol::{BatchValidationProvider, L2BlockInfo, to_system_config};
use http_body_util::Full;
use lru::LruCache;
use tower::ServiceBuilder;

use crate::Metrics;

const L2_BLOCK_REF_BY_NUMBER_METHOD: &str = "l2_block_ref_by_number";
const L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS: usize = 5;
const L2_BLOCK_VISIBILITY_RETRY_DELAY: Duration = Duration::from_millis(20);

/// The [`AlloyL2ChainProvider`] is a concrete implementation of the [`L2ChainProvider`] trait,
/// providing data over Ethereum JSON-RPC using an alloy provider as the backend.
#[derive(Debug, Clone)]
pub struct AlloyL2ChainProvider {
    /// The inner Ethereum JSON-RPC provider.
    inner: RootProvider<Base>,
    /// Whether to trust the RPC without verification.
    trust_rpc: bool,
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// The `block_by_number` LRU cache.
    block_by_number_cache: LruCache<u64, BaseBlock>,
}

impl AlloyL2ChainProvider {
    /// Creates a new [`AlloyL2ChainProvider`] with the given alloy provider and [`RollupConfig`].
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new(
        inner: RootProvider<Base>,
        rollup_config: Arc<RollupConfig>,
        cache_size: usize,
    ) -> Self {
        Self::new_with_trust(inner, rollup_config, cache_size, true)
    }

    /// Creates a new [`AlloyL2ChainProvider`] with the given alloy provider, [`RollupConfig`], and
    /// trust setting.
    ///
    /// ## Panics
    /// - Panics if `cache_size` is zero.
    pub fn new_with_trust(
        inner: RootProvider<Base>,
        rollup_config: Arc<RollupConfig>,
        cache_size: usize,
        trust_rpc: bool,
    ) -> Self {
        Self {
            inner,
            trust_rpc,
            rollup_config,
            block_by_number_cache: LruCache::new(NonZeroUsize::new(cache_size).unwrap()),
        }
    }

    /// Returns the chain ID.
    pub async fn chain_id(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        self.inner.get_chain_id().await
    }

    /// Returns the latest L2 block number.
    pub async fn latest_block_number(&mut self) -> Result<u64, RpcError<TransportErrorKind>> {
        self.inner.get_block_number().await
    }

    /// Verifies that a block's hash matches the expected hash when `trust_rpc` is false.
    fn verify_block_hash(
        &self,
        block_hash: B256,
        expected_hash: B256,
    ) -> Result<(), RpcError<TransportErrorKind>> {
        if self.trust_rpc {
            return Ok(());
        }

        if block_hash != expected_hash {
            return Err(RpcError::local_usage_str(&format!(
                "Block hash mismatch: expected {expected_hash:?}, got {block_hash:?}"
            )));
        }

        Ok(())
    }

    /// Returns the [`L2BlockInfo`] for the given [`BlockId`]. [None] is returned if the block
    /// does not exist.
    pub async fn block_info_by_id(
        &mut self,
        id: BlockId,
    ) -> Result<Option<L2BlockInfo>, RpcError<TransportErrorKind>> {
        let method_name = match id {
            BlockId::Number(_) => "l2_block_ref_by_number",
            BlockId::Hash(_) => "l2_block_ref_by_hash",
        };

        Metrics::l2_chain_requests(method_name).increment(1);

        let raw_block = base_metrics::time!(Metrics::request_duration(method_name), {
            match &id {
                BlockId::Number(num) => self.inner.get_block_by_number(*num).full().await,
                BlockId::Hash(hash) => self.inner.get_block_by_hash(hash.block_hash).full().await,
            }
        });

        let result = async {
            let block = match id {
                BlockId::Number(_) => raw_block?,
                BlockId::Hash(hash) => {
                    let block = raw_block?;

                    // Verify block hash matches if we fetched by hash
                    if let Some(ref b) = block {
                        self.verify_block_hash(b.header.hash, hash.block_hash)?;
                    }

                    block
                }
            };

            match block {
                Some(block) => {
                    let consensus_block = block
                        .map_header(|header| header.into_inner())
                        .into_consensus()
                        .map_transactions(|t| t.inner.inner);

                    let l2_block = L2BlockInfo::from_block_and_genesis(
                        &consensus_block,
                        &self.rollup_config.genesis,
                    )
                    .map_err(|_| {
                        RpcError::local_usage_str(
                            "failed to construct L2BlockInfo from block and genesis",
                        )
                    })?;
                    Ok(Some(l2_block))
                }
                None => Ok(None),
            }
        }
        .await;

        if result.is_err() {
            Metrics::l2_chain_errors(method_name).increment(1);
        }

        result
    }

    /// Creates a new [`AlloyL2ChainProvider`] from the provided [`url::Url`].
    pub fn new_http(
        url: url::Url,
        rollup_config: Arc<RollupConfig>,
        cache_size: usize,
        jwt: JwtSecret,
    ) -> Self {
        let hyper_client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();

        let auth_layer = AuthLayer::new(jwt);
        let service = ServiceBuilder::new().layer(auth_layer).service(hyper_client);

        let layer_transport = HyperClient::with_service(service);
        let http_hyper = Http::with_client(layer_transport, url);
        let rpc_client = RpcClient::new(http_hyper, false);

        let rpc = RootProvider::<Base>::new(rpc_client);
        Self::new(rpc, rollup_config, cache_size)
    }
}

/// An error for the [`AlloyL2ChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum AlloyL2ChainProviderError {
    /// Transport error
    #[error(transparent)]
    Transport(#[from] RpcError<TransportErrorKind>),
    /// Failed to find a block.
    #[error("Failed to fetch block {0}")]
    BlockNotFound(u64),
    /// Failed to construct [`L2BlockInfo`] from the block and genesis.
    #[error("Failed to construct L2BlockInfo from block {0} and genesis")]
    L2BlockInfoConstruction(u64),
    /// Failed to convert the block into a [`SystemConfig`].
    #[error("Failed to convert block {0} into SystemConfig")]
    SystemConfigConversion(u64),
}

impl From<AlloyL2ChainProviderError> for PipelineErrorKind {
    fn from(e: AlloyL2ChainProviderError) -> Self {
        match e {
            AlloyL2ChainProviderError::Transport(e) => {
                Self::Temporary(PipelineError::Provider(format!("Transport error: {e}")))
            }
            AlloyL2ChainProviderError::BlockNotFound(number) => {
                ResetError::BlockNotFound(alloy_eips::BlockId::Number(number.into())).reset()
            }
            AlloyL2ChainProviderError::L2BlockInfoConstruction(_) => Self::Temporary(
                PipelineError::Provider("L2 block info construction failed".to_string()),
            ),
            AlloyL2ChainProviderError::SystemConfigConversion(_) => Self::Temporary(
                PipelineError::Provider("system config conversion failed".to_string()),
            ),
        }
    }
}

#[async_trait]
impl BatchValidationProvider for AlloyL2ChainProvider {
    type Error = AlloyL2ChainProviderError;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        let block = self.block_by_number(number).await?;
        L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            .map_err(|_| AlloyL2ChainProviderError::L2BlockInfoConstruction(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, Self::Error> {
        if let Some(block) = self.block_by_number_cache.get(&number) {
            return Ok(block.clone());
        }

        for attempt in 1..=L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS {
            Metrics::l2_chain_requests(L2_BLOCK_REF_BY_NUMBER_METHOD).increment(1);

            let block =
                base_metrics::time!(Metrics::request_duration(L2_BLOCK_REF_BY_NUMBER_METHOD), {
                    self.inner.get_block_by_number(number.into()).full().await
                })
                .map_err(|e| {
                    Metrics::l2_chain_errors(L2_BLOCK_REF_BY_NUMBER_METHOD).increment(1);
                    AlloyL2ChainProviderError::Transport(e)
                })?;

            if let Some(block) = block {
                let block = block
                    .map_header(|header| header.into_inner())
                    .into_consensus()
                    .map_transactions(|t| t.inner.inner.into_inner());
                self.block_by_number_cache.put(number, block.clone());
                return Ok(block);
            }

            if attempt < L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS {
                Metrics::l2_block_visibility_retries().increment(1);
                tracing::debug!(
                    target: "l2_chain_provider",
                    number,
                    attempt,
                    attempts = L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS,
                    delay = ?L2_BLOCK_VISIBILITY_RETRY_DELAY,
                    "L2 block not visible yet; retrying"
                );
                tokio::time::sleep(L2_BLOCK_VISIBILITY_RETRY_DELAY).await;
            }
        }

        tracing::warn!(
            target: "l2_chain_provider",
            number,
            attempts = L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS,
            "L2 block not visible after exhausting retry budget"
        );
        Err(AlloyL2ChainProviderError::BlockNotFound(number))
    }
}

#[async_trait]
impl L2ChainProvider for AlloyL2ChainProvider {
    type Error = AlloyL2ChainProviderError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as BatchValidationProvider>::Error> {
        let block = self.block_by_number(number).await?;
        to_system_config(&block, &rollup_config)
            .map_err(|_| AlloyL2ChainProviderError::SystemConfigConversion(number))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_provider::RootProvider;
    use httpmock::{HttpMockRequest, HttpMockResponse, Method::POST, MockServer};
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn test_from_alloy_l2_chain_provider_error() {
        // Transport errors are transient — retry makes sense.
        let kind: PipelineErrorKind =
            AlloyL2ChainProviderError::Transport(alloy_transport::RpcError::Transport(
                alloy_transport::TransportErrorKind::Custom("timeout".into()),
            ))
            .into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // L2BlockInfoConstruction is a decode failure — transient.
        let kind: PipelineErrorKind = AlloyL2ChainProviderError::L2BlockInfoConstruction(0).into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // SystemConfigConversion is a decode failure — transient.
        let kind: PipelineErrorKind = AlloyL2ChainProviderError::SystemConfigConversion(0).into();
        assert!(matches!(kind, PipelineErrorKind::Temporary(_)));

        // L2 BlockNotFound: the pipeline only requests blocks that should exist on the
        // canonical chain. A missing L2 block means a reorg occurred — must Reset.
        let kind: PipelineErrorKind = AlloyL2ChainProviderError::BlockNotFound(42).into();
        assert!(
            matches!(kind, PipelineErrorKind::Reset(_)),
            "L2 BlockNotFound must map to Reset (block disappeared due to reorg)"
        );
    }

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

    fn json_rpc_error_response(req: &HttpMockRequest) -> String {
        let id = serde_json::from_slice::<Value>(&req.body_vec())
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(Value::Null);
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": "backend unavailable" }
        })
        .to_string()
    }

    fn l2_provider(server: &MockServer) -> AlloyL2ChainProvider {
        AlloyL2ChainProvider::new(
            RootProvider::<Base>::new_http(server.url("/").parse().unwrap()),
            Arc::new(RollupConfig::default()),
            16,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn test_block_by_number_retries_nulls_then_succeeds() {
        let server = MockServer::start_async().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let block_number = 42;
        let block = block_json(block_number);
        let hits_clone = Arc::clone(&hits);
        let mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.respond_with(move |req| {
                    let hit = hits_clone.fetch_add(1, Ordering::SeqCst);
                    let result = if hit < 2 { Value::Null } else { block.clone() };
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, result))
                        .build()
                });
            })
            .await;

        let mut provider = l2_provider(&server);
        let block = provider.block_by_number(block_number).await.unwrap();

        assert_eq!(block.header.number, block_number);
        mock.assert_calls_async(3).await;
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_block_by_number_exhausts_null_retry_budget() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.respond_with(|req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_response(req, Value::Null))
                        .build()
                });
            })
            .await;

        let mut provider = l2_provider(&server);
        let err = provider.block_by_number(42).await.unwrap_err();

        assert!(matches!(err, AlloyL2ChainProviderError::BlockNotFound(42)));
        mock.assert_calls_async(L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_block_by_number_transport_error_does_not_retry() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.respond_with(|req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_error_response(req))
                        .build()
                });
            })
            .await;

        let mut provider = l2_provider(&server);
        let err = provider.block_by_number(42).await.unwrap_err();

        assert!(matches!(err, AlloyL2ChainProviderError::Transport(_)));
        mock.assert_calls_async(1).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_block_by_number_callers_preserve_transport_errors() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.respond_with(|req| {
                    HttpMockResponse::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json_rpc_error_response(req))
                        .build()
                });
            })
            .await;

        let mut provider = l2_provider(&server);
        let err = provider.l2_block_info_by_number(42).await.unwrap_err();
        assert!(matches!(err, AlloyL2ChainProviderError::Transport(_)));

        let err = provider
            .system_config_by_number(42, Arc::new(RollupConfig::default()))
            .await
            .unwrap_err();
        assert!(matches!(err, AlloyL2ChainProviderError::Transport(_)));

        mock.assert_calls_async(2).await;
    }
}
