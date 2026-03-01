//! Ethereum utilities for the host.

use alloy_provider::{Network, RootProvider};

/// Returns an HTTP provider for the given URL.
pub async fn rpc_provider<N: Network>(url: &str) -> RootProvider<N> {
    RootProvider::connect(url).await.unwrap()
}
