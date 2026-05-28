//! Providers that use alloy provider types on the backend.

use std::{future::Future, num::NonZeroUsize, sync::Arc, time::Duration};

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
                    let consensus_block =
                        block.into_consensus().map_transactions(|t| t.inner.inner);

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

fn l2_block_visibility_retry_delay(attempt: usize) -> Option<Duration> {
    (attempt + 1 < L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS).then_some(L2_BLOCK_VISIBILITY_RETRY_DELAY)
}

// The local execution RPC can briefly return `null` for a just-imported L2 block while the
// canonical block is still becoming visible through the RPC cache/DB path.
async fn fetch_l2_block_with_visibility_retries<F, Fut, Sleep, SleepFut>(
    number: u64,
    mut fetch: F,
    mut sleep: Sleep,
) -> Result<Option<BaseBlock>, AlloyL2ChainProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<BaseBlock>, AlloyL2ChainProviderError>>,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
{
    for attempt in 0..L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS {
        if let Some(block) = fetch().await? {
            return Ok(Some(block));
        }

        if let Some(delay) = l2_block_visibility_retry_delay(attempt) {
            Metrics::l2_block_visibility_retries().increment(1);
            tracing::debug!(
                target: "l2_chain_provider",
                number,
                attempt = attempt + 1,
                attempts = L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS,
                ?delay,
                "L2 block not visible yet; retrying"
            );
            sleep(delay).await;
        }
    }

    Ok(None)
}

async fn fetch_block_by_number_once(
    inner: RootProvider<Base>,
    number: u64,
) -> Result<Option<BaseBlock>, AlloyL2ChainProviderError> {
    Metrics::l2_chain_requests(L2_BLOCK_REF_BY_NUMBER_METHOD).increment(1);

    base_metrics::time!(Metrics::request_duration(L2_BLOCK_REF_BY_NUMBER_METHOD), {
        inner.get_block_by_number(number.into()).full().await
    })
    .map_err(|e| {
        Metrics::l2_chain_errors(L2_BLOCK_REF_BY_NUMBER_METHOD).increment(1);
        AlloyL2ChainProviderError::Transport(e)
    })
    .map(|block| {
        block.map(|block| block.into_consensus().map_transactions(|t| t.inner.inner.into_inner()))
    })
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
        let block = self
            .block_by_number(number)
            .await
            .map_err(|_| AlloyL2ChainProviderError::BlockNotFound(number))?;
        L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            .map_err(|_| AlloyL2ChainProviderError::L2BlockInfoConstruction(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, Self::Error> {
        if let Some(block) = self.block_by_number_cache.get(&number) {
            return Ok(block.clone());
        }

        let inner = self.inner.clone();
        let block = fetch_l2_block_with_visibility_retries(
            number,
            || {
                let inner = inner.clone();
                async move { fetch_block_by_number_once(inner, number).await }
            },
            tokio::time::sleep,
        )
        .await?
        .ok_or(AlloyL2ChainProviderError::BlockNotFound(number))?;

        self.block_by_number_cache.put(number, block.clone());
        Ok(block)
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
        let block = self
            .block_by_number(number)
            .await
            .map_err(|_| AlloyL2ChainProviderError::BlockNotFound(number))?;
        to_system_config(&block, &rollup_config)
            .map_err(|_| AlloyL2ChainProviderError::SystemConfigConversion(number))
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_l2_block_visibility_retry_delay() {
        assert_eq!(l2_block_visibility_retry_delay(0), Some(L2_BLOCK_VISIBILITY_RETRY_DELAY));
        assert_eq!(
            l2_block_visibility_retry_delay(L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS - 2),
            Some(L2_BLOCK_VISIBILITY_RETRY_DELAY)
        );
        assert_eq!(l2_block_visibility_retry_delay(L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS - 1), None);
    }

    #[tokio::test]
    async fn test_l2_block_visibility_retry_succeeds_after_transient_misses() {
        let mut attempts = 0;
        let mut sleeps = 0;

        let result = fetch_l2_block_with_visibility_retries(
            42,
            || {
                attempts += 1;
                let block = (attempts == 3).then(BaseBlock::default);
                std::future::ready(Ok(block))
            },
            |_| {
                sleeps += 1;
                std::future::ready(())
            },
        )
        .await
        .unwrap();

        assert!(result.is_some());
        assert_eq!(attempts, 3);
        assert_eq!(sleeps, 2);
    }

    #[tokio::test]
    async fn test_l2_block_visibility_retry_stops_after_attempt_budget() {
        let mut attempts = 0;
        let mut sleeps = 0;

        let result = fetch_l2_block_with_visibility_retries(
            42,
            || {
                attempts += 1;
                std::future::ready(Ok(None))
            },
            |_| {
                sleeps += 1;
                std::future::ready(())
            },
        )
        .await
        .unwrap();

        assert!(result.is_none());
        assert_eq!(attempts, L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS);
        assert_eq!(sleeps, L2_BLOCK_VISIBILITY_RETRY_ATTEMPTS - 1);
    }

    #[tokio::test]
    async fn test_l2_block_visibility_retry_returns_errors_without_retrying() {
        let mut attempts = 0;
        let mut sleeps = 0;

        let err = fetch_l2_block_with_visibility_retries(
            42,
            || {
                attempts += 1;
                std::future::ready(Err(AlloyL2ChainProviderError::BlockNotFound(42)))
            },
            |_| {
                sleeps += 1;
                std::future::ready(())
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AlloyL2ChainProviderError::BlockNotFound(42)));
        assert_eq!(attempts, 1);
        assert_eq!(sleeps, 0);
    }
}
