//! Rollup RPC client implementation for OP Stack rollup nodes.

use std::time::Duration;

use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::transports::http::{Http, reqwest::Client};
use async_trait::async_trait;
use backon::Retryable;
use op_enclave_core::types::config::RollupConfig;
use serde_json::Value;
use url::Url;

use super::{
    HttpProvider,
    error::{RpcError, RpcResult},
    traits::RollupClient,
    types::SyncStatus,
};
use crate::config::RetryConfig;

/// Configuration for the rollup client.
#[derive(Debug, Clone)]
pub struct RollupClientConfig {
    /// RPC endpoint URL.
    pub endpoint: Url,
    /// Request timeout.
    pub timeout: Duration,
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
            retry_config: RetryConfig::default(),
            skip_tls_verify: false,
        }
    }

    /// Sets the request timeout.
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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
pub struct RollupClientImpl {
    /// The underlying HTTP provider.
    provider: HttpProvider,
    /// Retry configuration.
    retry_config: RetryConfig,
}

impl std::fmt::Debug for RollupClientImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollupClientImpl").finish_non_exhaustive()
    }
}

impl RollupClientImpl {
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

        Ok(Self {
            provider,
            retry_config: config.retry_config,
        })
    }
}

#[async_trait]
impl RollupClient for RollupClientImpl {
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
