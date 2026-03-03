//! Ethereum RPC provider utilities for the host.

use alloy_provider::{Network, RootProvider};
use alloy_transport::TransportError;

/// Factory for creating RPC providers.
#[derive(Debug)]
pub struct RpcProviderFactory;

impl RpcProviderFactory {
    /// Returns an HTTP provider for the given URL.
    pub async fn connect<N: Network>(url: &str) -> Result<RootProvider<N>, TransportError> {
        RootProvider::connect(url).await
    }
}
