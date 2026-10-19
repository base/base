//! Shared HTTP providers for consensus L1 execution JSON-RPC requests.

use std::time::Duration;

use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use reqwest::Client;
use url::Url;

/// Default request timeout for general consensus L1 execution JSON-RPC clients.
///
/// Budget: this must stay well under the shortest deadline of a synchronous caller that waits on
/// an L1 execution request. The tightest known caller is op-conductor's health check, which
/// waits at most 10s on each `optimism_syncStatus` call; an inner L1 deadline at or above that
/// turns a stalled L1 endpoint into a failed health check on every node at once. 5s also stays
/// at or under proxyd's default 5s backend timeout, so the proxy rather than this client reports
/// a stalled backend first.
///
/// Requests that can carry large responses tolerate this deadline rather than needing a longer
/// one: when a derivation receipt or full-block fetch times out, the pipeline yields and retries
/// it on its next trigger, such as the next L1 head notification, so a slow request delays
/// derivation instead of failing it. The sequencer hot path uses its own, shorter deadline.
pub const L1_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Constructs standard consensus L1 HTTP providers.
#[derive(Debug, Clone, Copy, Default)]
pub struct L1RpcProvider;

impl L1RpcProvider {
    /// Creates an L1 execution JSON-RPC provider with the default request deadline.
    pub fn new_http(url: Url) -> RootProvider {
        Self::new_http_with_timeout(url, L1_RPC_TIMEOUT)
    }

    /// Creates an L1 execution JSON-RPC provider with the provided request deadline.
    pub fn new_http_with_timeout(url: Url, timeout: Duration) -> RootProvider {
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

    fn is_reqwest_timeout<T>(result: &alloy_transport::TransportResult<T>) -> bool {
        result
            .as_ref()
            .err()
            .and_then(|error| error.as_transport_err())
            .and_then(|error| error.as_custom())
            .and_then(|error| error.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
    }

    #[tokio::test]
    async fn execution_requests_timeout_at_the_provider_boundary() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).delay(TEST_RESPONSE_DELAY);
            })
            .await;
        let provider = L1RpcProvider::new_http_with_timeout(
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

        assert!(is_reqwest_timeout(&requests.0), "eth_getLogs should fail with a request timeout");
        assert!(
            is_reqwest_timeout(&requests.1),
            "eth_getBlockByNumber should fail with a request timeout"
        );
        mock.assert_calls_async(2).await;
    }

    #[tokio::test]
    async fn execution_providers_can_use_independent_request_timeouts() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).delay(TEST_RESPONSE_DELAY).json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": "0x1"
                }));
            })
            .await;
        let url: Url = server.url("/").parse().expect("mock server URL must parse");
        let short_timeout_provider =
            L1RpcProvider::new_http_with_timeout(url.clone(), TEST_REQUEST_TIMEOUT);
        let long_timeout_provider =
            L1RpcProvider::new_http_with_timeout(url, Duration::from_millis(500));

        let short_result = short_timeout_provider.get_block_number().await;
        let long_result = long_timeout_provider.get_block_number().await;

        assert!(is_reqwest_timeout(&short_result));
        assert_eq!(long_result.unwrap(), 1);
        mock.assert_calls_async(2).await;
    }
}
