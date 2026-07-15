use std::time::Duration;

use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use reqwest::Client;
use url::Url;

/// Maximum duration for every HTTP request made by a consensus L1 client.
///
/// This deadline is shared by L1 execution JSON-RPC and Beacon API requests. It is intentionally
/// not operation-specific: all standard consensus L1 clients use the same transport deadline.
pub const L1_RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Constructs standard consensus L1 HTTP providers.
#[derive(Debug, Clone, Copy, Default)]
pub struct L1RpcProvider;

impl L1RpcProvider {
    /// Creates an L1 execution JSON-RPC provider with the shared request deadline.
    pub fn new_http(url: Url) -> RootProvider {
        let client = Client::builder()
            .timeout(L1_RPC_TIMEOUT)
            .build()
            .expect("failed to build L1 RPC HTTP client");

        RootProvider::new(RpcClient::new_http_with_client(client, url))
    }
}
