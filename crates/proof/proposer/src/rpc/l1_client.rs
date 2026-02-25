//! L1 RPC client implementation.

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{
    BlockId, Header, TransactionInput, TransactionReceipt, TransactionRequest,
};
use alloy_transport_http::{Http, reqwest::Client};
use async_trait::async_trait;
use backon::Retryable;
use url::Url;

use super::{
    HttpProvider,
    cache::MeteredCache,
    error::{RpcError, RpcResult},
    traits::L1Client,
};
use crate::{config::RetryConfig, constants::DEFAULT_CACHE_SIZE};

/// Configuration for the L1 client.
#[derive(Debug, Clone)]
pub struct L1ClientConfig {
    /// RPC endpoint URL.
    pub endpoint: Url,
    /// Request timeout.
    pub timeout: Duration,
    /// Cache size for headers and receipts.
    pub cache_size: usize,
    /// Retry configuration.
    pub retry_config: RetryConfig,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: bool,
}

impl L1ClientConfig {
    /// Creates a new L1 client configuration with defaults.
    pub fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            timeout: Duration::from_secs(30),
            cache_size: DEFAULT_CACHE_SIZE,
            retry_config: RetryConfig::default(),
            skip_tls_verify: false,
        }
    }

    /// Sets the request timeout.
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the cache size.
    pub const fn with_cache_size(mut self, cache_size: usize) -> Self {
        self.cache_size = cache_size;
        self
    }

    /// Sets the retry configuration.
    pub const fn with_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }

    /// Sets whether to skip TLS certificate verification.
    pub const fn with_skip_tls_verify(mut self, skip: bool) -> Self {
        self.skip_tls_verify = skip;
        self
    }
}

/// L1 RPC client implementation using Alloy.
pub struct L1ClientImpl {
    /// The underlying HTTP provider.
    provider: HttpProvider,
    /// Cache for headers by hash.
    headers_cache: MeteredCache<B256, Header>,
    /// Cache for receipts by block hash.
    receipts_cache: MeteredCache<B256, Vec<TransactionReceipt>>,
    /// Retry configuration.
    retry_config: RetryConfig,
}

impl std::fmt::Debug for L1ClientImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L1ClientImpl")
            .field("headers_cache_entries", &self.headers_cache.entry_count())
            .field("receipts_cache_entries", &self.receipts_cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl L1ClientImpl {
    /// Creates a new L1 client from the given configuration.
    pub fn new(config: L1ClientConfig) -> RpcResult<Self> {
        // Create reqwest Client with timeout
        let mut builder = Client::builder().timeout(config.timeout);

        if config.skip_tls_verify {
            tracing::warn!("TLS certificate verification is disabled for L1 RPC connection");
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder
            .build()
            .map_err(|e| RpcError::Connection(format!("Failed to build HTTP client: {e}")))?;

        // Create HTTP transport with custom client
        let http = Http::with_client(client, config.endpoint);
        let rpc_client = RpcClient::new(http, false);

        // Create provider directly without fillers (read-only operations)
        let provider = RootProvider::new(rpc_client);

        Ok(Self {
            provider,
            headers_cache: MeteredCache::with_capacity("l1_headers", config.cache_size),
            receipts_cache: MeteredCache::with_capacity("l1_receipts", config.cache_size),
            retry_config: config.retry_config,
        })
    }

    /// Returns the headers cache metrics.
    pub const fn headers_cache(&self) -> &MeteredCache<B256, Header> {
        &self.headers_cache
    }

    /// Returns the receipts cache metrics.
    pub const fn receipts_cache(&self) -> &MeteredCache<B256, Vec<TransactionReceipt>> {
        &self.receipts_cache
    }
}

#[async_trait]
impl L1Client for L1ClientImpl {
    async fn block_number(&self) -> RpcResult<u64> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async { self.provider.get_block_number().await.map_err(RpcError::from) })
            .retry(backoff)
            .when(|e| e.is_retryable())
            .notify(|err, dur| {
                tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::block_number");
            })
            .await
    }

    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header> {
        let block_id: BlockId =
            number.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number).into();

        let backoff = self.retry_config.to_backoff_builder();

        let block = (|| async { self.provider.get_block(block_id).await.map_err(RpcError::from) })
            .retry(backoff)
            .when(|e| e.is_retryable())
            .notify(|err, dur| {
                tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::header_by_number");
            })
            .await?
            .ok_or_else(|| {
                RpcError::HeaderNotFound(format!("Header not found for {block_id:?}"))
            })?;

        let header = block.header;

        // Cache the header by its hash
        self.headers_cache.insert(header.hash, header.clone()).await;

        Ok(header)
    }

    async fn header_by_hash(&self, hash: B256) -> RpcResult<Header> {
        // Check cache first
        if let Some(header) = self.headers_cache.get(&hash).await {
            return Ok(header);
        }

        let backoff = self.retry_config.to_backoff_builder();

        let block = (|| async {
            self.provider.get_block(BlockId::Hash(hash.into())).await.map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::header_by_hash");
        })
        .await?
        .ok_or_else(|| RpcError::HeaderNotFound(format!("Header not found for hash {hash}")))?;

        let header = block.header;

        // Cache the result
        self.headers_cache.insert(hash, header.clone()).await;

        Ok(header)
    }

    async fn block_receipts(&self, hash: B256) -> RpcResult<Vec<TransactionReceipt>> {
        // Check cache first
        if let Some(receipts) = self.receipts_cache.get(&hash).await {
            return Ok(receipts);
        }

        let backoff = self.retry_config.to_backoff_builder();

        let receipts = (|| async {
            self.provider
                .get_block_receipts(BlockId::Hash(hash.into()))
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::block_receipts");
        })
        .await?
        .ok_or_else(|| {
            RpcError::BlockNotFound(format!("Block receipts not found for hash {hash}"))
        })?;

        // Cache the result
        self.receipts_cache.insert(hash, receipts.clone()).await;

        Ok(receipts)
    }

    async fn code_at(&self, address: Address, block_number: Option<u64>) -> RpcResult<Bytes> {
        let block_id = BlockId::Number(
            block_number.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number),
        );

        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            self.provider.get_code_at(address).block_id(block_id).await.map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::code_at");
        })
        .await
    }

    async fn call_contract(
        &self,
        to: Address,
        data: Bytes,
        block_number: Option<u64>,
    ) -> RpcResult<Bytes> {
        let block_id = BlockId::Number(
            block_number.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number),
        );

        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            let req =
                TransactionRequest::default().to(to).input(TransactionInput::new(data.clone()));
            self.provider.call(req).block(block_id).await.map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::call_contract");
        })
        .await
    }

    async fn get_balance(&self, address: Address) -> RpcResult<U256> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async { self.provider.get_balance(address).await.map_err(RpcError::from) })
            .retry(backoff)
            .when(|e| e.is_retryable())
            .notify(|err, dur| {
                tracing::debug!(error = %err, delay = ?dur, "Retrying L1Client::get_balance");
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_client_config_defaults() {
        let config = L1ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.cache_size, DEFAULT_CACHE_SIZE);
    }

    #[test]
    fn test_l1_client_config_builder() {
        let config = L1ClientConfig::new(Url::parse("http://localhost:8545").unwrap())
            .with_timeout(Duration::from_secs(60))
            .with_cache_size(500);

        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.cache_size, 500);
    }
}
