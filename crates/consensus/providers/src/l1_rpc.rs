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
        Self::with_timeout(url, L1_RPC_TIMEOUT)
    }

    fn with_timeout(url: Url, timeout: Duration) -> RootProvider {
        let client =
            Client::builder().timeout(timeout).build().expect("failed to build L1 RPC HTTP client");

        RootProvider::new(RpcClient::new_http_with_client(client, url))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_eips::BlockId;
    use alloy_provider::Provider;
    use alloy_rpc_types_eth::Filter;
    use httpmock::prelude::*;

    use super::*;

    const TEST_REQUEST_TIMEOUT: Duration = Duration::from_millis(25);
    const TEST_RESPONSE_DELAY: Duration = Duration::from_millis(250);

    #[tokio::test]
    async fn execution_requests_timeout_at_the_provider_boundary() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).delay(TEST_RESPONSE_DELAY);
            })
            .await;
        let provider = L1RpcProvider::with_timeout(
            server.url("/").parse().expect("mock server URL must parse"),
            TEST_REQUEST_TIMEOUT,
        );

        let requests = tokio::time::timeout(Duration::from_millis(500), async {
            let logs = provider.get_logs(&Filter::new()).await;
            let block = provider.get_block(BlockId::Number(1u64.into())).await;
            (logs, block)
        })
        .await
        .expect("provider requests must not remain pending");

        assert!(requests.0.is_err(), "eth_getLogs should fail at the request deadline");
        assert!(requests.1.is_err(), "eth_getBlockByNumber should fail at the request deadline");
        mock.assert_calls_async(2).await;
    }
}
