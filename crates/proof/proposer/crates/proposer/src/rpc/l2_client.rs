//! L2 RPC client implementation.

use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::transports::http::{Http, reqwest::Client};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::{BlockId, Header};
use async_trait::async_trait;
use backon::Retryable;
use op_enclave_core::{AccountResult, executor::ExecutionWitness};
use url::Url;

use super::{
    L2HttpProvider,
    cache::MeteredCache,
    error::{RpcError, RpcResult},
    traits::L2Client,
    types::OpBlock,
};
use crate::config::RetryConfig;
use crate::constants::DEFAULT_CACHE_SIZE;

/// Cache key for account proofs (address + block hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProofCacheKey {
    /// Account address.
    pub address: Address,
    /// Block hash.
    pub block_hash: B256,
}

impl ProofCacheKey {
    /// Creates a new proof cache key.
    pub const fn new(address: Address, block_hash: B256) -> Self {
        Self {
            address,
            block_hash,
        }
    }
}

/// Configuration for the L2 client.
#[derive(Debug, Clone)]
pub struct L2ClientConfig {
    /// RPC endpoint URL.
    pub endpoint: Url,
    /// Request timeout.
    pub timeout: Duration,
    /// Cache size for blocks, headers, and proofs.
    pub cache_size: usize,
    /// Retry configuration.
    pub retry_config: RetryConfig,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: bool,
}

impl L2ClientConfig {
    /// Creates a new L2 client configuration with defaults.
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

/// L2 RPC client implementation using Alloy.
pub struct L2ClientImpl {
    /// The underlying HTTP provider (Optimism network for deposit tx support).
    provider: L2HttpProvider,
    /// Cache for blocks by hash.
    blocks_cache: MeteredCache<B256, OpBlock>,
    /// Cache for headers by hash.
    headers_cache: MeteredCache<B256, Header>,
    /// Cache for account proofs.
    proofs_cache: MeteredCache<ProofCacheKey, AccountResult>,
    /// Retry configuration.
    retry_config: RetryConfig,
}

impl std::fmt::Debug for L2ClientImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2ClientImpl")
            .field("blocks_cache_entries", &self.blocks_cache.entry_count())
            .field("headers_cache_entries", &self.headers_cache.entry_count())
            .field("proofs_cache_entries", &self.proofs_cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl L2ClientImpl {
    /// Creates a new L2 client from the given configuration.
    pub fn new(config: L2ClientConfig) -> RpcResult<Self> {
        // Create reqwest Client with timeout
        let mut builder = Client::builder().timeout(config.timeout);

        if config.skip_tls_verify {
            tracing::warn!("TLS certificate verification is disabled for L2 RPC connection");
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
            blocks_cache: MeteredCache::with_capacity("l2_blocks", config.cache_size),
            headers_cache: MeteredCache::with_capacity("l2_headers", config.cache_size),
            proofs_cache: MeteredCache::with_capacity("l2_proofs", config.cache_size),
            retry_config: config.retry_config,
        })
    }

    /// Returns the blocks cache.
    pub const fn blocks_cache(&self) -> &MeteredCache<B256, OpBlock> {
        &self.blocks_cache
    }

    /// Returns the headers cache.
    pub const fn headers_cache(&self) -> &MeteredCache<B256, Header> {
        &self.headers_cache
    }

    /// Returns the proofs cache.
    pub const fn proofs_cache(&self) -> &MeteredCache<ProofCacheKey, AccountResult> {
        &self.proofs_cache
    }

    /// Returns a reference to the underlying provider.
    pub(crate) const fn provider(&self) -> &L2HttpProvider {
        &self.provider
    }

    /// Returns a reference to the retry configuration.
    pub(crate) const fn retry_config(&self) -> &RetryConfig {
        &self.retry_config
    }
}

#[async_trait]
impl L2Client for L2ClientImpl {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            self.provider
                .raw_request::<_, serde_json::Value>("debug_chainConfig".into(), ())
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::chain_config");
        })
        .await
    }

    async fn get_proof(&self, address: Address, block_hash: B256) -> RpcResult<AccountResult> {
        let cache_key = ProofCacheKey::new(address, block_hash);

        // Check cache first
        if let Some(proof) = self.proofs_cache.get(&cache_key).await {
            return Ok(proof);
        }

        // Make the RPC call with empty storage keys
        let empty_keys: Vec<B256> = vec![];
        let backoff = self.retry_config.to_backoff_builder();

        let proof: AccountResult = (|| async {
            self.provider
                .raw_request(
                    "eth_getProof".into(),
                    (address, empty_keys.clone(), block_hash),
                )
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::get_proof");
        })
        .await?;

        // Cache the result
        self.proofs_cache.insert(cache_key, proof.clone()).await;

        Ok(proof)
    }

    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header> {
        let block_id: BlockId = number
            .map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number)
            .into();

        let backoff = self.retry_config.to_backoff_builder();

        let block = (|| async {
            self.provider
                .get_block(block_id)
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::header_by_number");
        })
        .await?
        .ok_or_else(|| RpcError::HeaderNotFound(format!("Header not found for {block_id:?}")))?;

        let header = block.header;

        // Cache by hash
        self.headers_cache.insert(header.hash, header.clone()).await;

        Ok(header)
    }

    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
        let block_id: BlockId = number
            .map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number)
            .into();

        let backoff = self.retry_config.to_backoff_builder();

        let block = (|| async {
            self.provider
                .get_block(block_id)
                .full()
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::block_by_number");
        })
        .await?
        .ok_or_else(|| RpcError::BlockNotFound(format!("Block not found for {block_id:?}")))?;

        // Cache by hash
        self.blocks_cache
            .insert(block.header.hash, block.clone())
            .await;
        self.headers_cache
            .insert(block.header.hash, block.header.clone())
            .await;

        Ok(block)
    }

    async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
        // Check cache first
        if let Some(block) = self.blocks_cache.get(&hash).await {
            return Ok(block);
        }

        let backoff = self.retry_config.to_backoff_builder();

        let block = (|| async {
            self.provider
                .get_block(BlockId::Hash(hash.into()))
                .full()
                .await
                .map_err(RpcError::from)
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::block_by_hash");
        })
        .await?
        .ok_or_else(|| RpcError::BlockNotFound(format!("Block not found for hash {hash}")))?;

        // Cache the result
        self.blocks_cache.insert(hash, block.clone()).await;
        self.headers_cache.insert(hash, block.header.clone()).await;

        Ok(block)
    }

    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            self.provider
                .raw_request::<_, ExecutionWitness>(
                    "debug_executionWitness".into(),
                    (BlockNumberOrTag::Number(block_number),),
                )
                .await
                .map_err(|e| RpcError::WitnessNotFound(format!("Block {block_number}: {e}")))
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::execution_witness");
        })
        .await
    }

    async fn db_get(&self, key: B256) -> RpcResult<alloy_primitives::Bytes> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            self.provider
                .raw_request::<_, alloy_primitives::Bytes>("debug_dbGet".into(), (key,))
                .await
                .map_err(|e| RpcError::InvalidResponse(format!("Failed to db_get key {key}: {e}")))
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying L2Client::db_get");
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l2_client_config_defaults() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.cache_size, DEFAULT_CACHE_SIZE);
    }

    #[test]
    fn test_l2_client_config_builder() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap())
            .with_timeout(Duration::from_secs(60))
            .with_cache_size(500);

        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.cache_size, 500);
    }

    #[test]
    fn test_proof_cache_key() {
        let addr1 = Address::ZERO;
        let addr2 = Address::repeat_byte(1);
        let hash1 = B256::ZERO;
        let hash2 = B256::repeat_byte(1);

        let key1 = ProofCacheKey::new(addr1, hash1);
        let key2 = ProofCacheKey::new(addr1, hash1);
        let key3 = ProofCacheKey::new(addr2, hash1);
        let key4 = ProofCacheKey::new(addr1, hash2);

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
        assert_ne!(key1, key4);
    }
}
