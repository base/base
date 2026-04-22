//! Rollup RPC client implementation for Base rollup nodes.

use std::time::Duration;

use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::{Http, reqwest::Client};
use async_trait::async_trait;
use backon::Retryable;
use base_common_genesis::RollupConfig;
use serde_json::Value;
use url::Url;

use super::{
    HttpProvider,
    cache::MeteredCache,
    config::{DEFAULT_CACHE_SIZE, RetryConfig},
    error::{RpcError, RpcResult},
    traits::RollupProvider,
    types::{OutputAtBlock, SyncStatus},
};

/// Configuration for the rollup client.
#[derive(Debug, Clone)]
pub struct RollupClientConfig {
    /// RPC endpoint URL.
    pub endpoint: Url,
    /// Request timeout.
    pub timeout: Duration,
    /// Cache size for output-at-block responses.
    pub cache_size: usize,
    /// Retry configuration.
    pub retry_config: RetryConfig,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: bool,
}

impl RollupClientConfig {
    /// Creates a new rollup client configuration with defaults.
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

    /// Sets the cache size for output-at-block responses.
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

/// Rollup RPC client implementation using Alloy.
pub struct RollupClient {
    /// The underlying HTTP provider.
    provider: HttpProvider,
    /// Cache for `optimism_outputAtBlock` responses keyed by L2 block number.
    output_cache: MeteredCache<u64, OutputAtBlock>,
    /// Retry configuration.
    retry_config: RetryConfig,
}

impl std::fmt::Debug for RollupClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollupClient")
            .field("output_cache_entries", &self.output_cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl RollupClient {
    /// Creates a new rollup client from the given configuration.
    pub fn new(config: RollupClientConfig) -> RpcResult<Self> {
        // Create reqwest Client with timeout
        let mut builder = Client::builder().timeout(config.timeout);

        if config.skip_tls_verify {
            tracing::warn!("TLS certificate verification is disabled for rollup RPC connection");
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

        let output_cache = MeteredCache::with_capacity("rollup_output_at_block", config.cache_size);

        Ok(Self { provider, output_cache, retry_config: config.retry_config })
    }

    /// Returns the output-at-block cache.
    pub const fn output_cache(&self) -> &MeteredCache<u64, OutputAtBlock> {
        &self.output_cache
    }
}

#[async_trait]
impl RollupProvider for RollupClient {
    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            let raw_response = self
                .provider
                .raw_request::<_, Value>("optimism_rollupConfig".into(), ())
                .await
                .map_err(|e| RpcError::InvalidResponse(format!("Failed to get rollup config: {e}")))?;

            tracing::debug!(raw_response = %raw_response, "Received raw optimism_rollupConfig response");

            serde_json::from_value(raw_response).map_err(|e| {
                RpcError::InvalidResponse(format!("Failed to deserialize rollup config: {e}"))
            })
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying RollupClient::rollup_config");
        })
        .await
    }

    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        let backoff = self.retry_config.to_backoff_builder();

        (|| async {
            self.provider
                .raw_request::<_, SyncStatus>("optimism_syncStatus".into(), ())
                .await
                .map_err(|e| RpcError::InvalidResponse(format!("Failed to get sync status: {e}")))
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying RollupClient::sync_status");
        })
        .await
    }

    async fn output_at_block(&self, block_number: u64) -> RpcResult<OutputAtBlock> {
        if let Some(cached) = self.output_cache.get(&block_number).await {
            return Ok(cached);
        }

        let backoff = self.retry_config.to_backoff_builder();

        let output = (|| async {
            self.provider
                .raw_request::<_, OutputAtBlock>(
                    "optimism_outputAtBlock".into(),
                    (format!("0x{block_number:x}"),),
                )
                .await
                .map_err(|e| {
                    RpcError::InvalidResponse(format!("Failed to get output at block: {e}"))
                })
        })
        .retry(backoff)
        .when(|e| e.is_retryable())
        .notify(|err, dur| {
            tracing::debug!(error = %err, delay = ?dur, "Retrying RollupClient::output_at_block");
        })
        .await?;

        self.output_cache.insert(block_number, output).await;

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rollup_client_config_defaults() {
        let config = RollupClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        assert_eq!(config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_rollup_client_config_builder() {
        let config = RollupClientConfig::new(Url::parse("http://localhost:8545").unwrap())
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.timeout, Duration::from_secs(60));
    }
}
