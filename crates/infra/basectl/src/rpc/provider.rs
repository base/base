//! Shared RPC provider construction helpers.

use std::time::Duration;

use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::Http;
use anyhow::{Context, Result};
use base_common_network::Base;
use url::Url;

/// Factory for RPC providers with basectl's standard HTTP request settings.
#[derive(Debug, Clone, Copy)]
pub struct RpcProviderFactory;

impl RpcProviderFactory {
    /// HTTP request timeout used for direct JSON-RPC requests.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Connects an L1 provider with an explicit HTTP request timeout.
    pub fn connect_l1_provider(rpc: &Url) -> Result<impl Provider> {
        let http_client = alloy_transport_http::reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .with_context(|| format!("building L1 HTTP client for {rpc}"))?;
        let transport = Http::with_client(http_client, rpc.clone());
        Ok(ProviderBuilder::new().connect_client(RpcClient::new(transport, false)))
    }

    /// Connects an L2 Base provider with an explicit HTTP request timeout.
    pub fn connect_l2_provider(rpc: &Url) -> Result<impl Provider<Base>> {
        let http_client = alloy_transport_http::reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .with_context(|| format!("building L2 HTTP client for {rpc}"))?;
        let transport = Http::with_client(http_client, rpc.clone());
        Ok(ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_client(RpcClient::new(transport, false)))
    }
}
