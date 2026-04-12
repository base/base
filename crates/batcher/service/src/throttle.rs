//! Production [`ThrottleClient`] that calls `miner_setMaxDASize` via jsonrpsee.

use alloy_primitives::U64;
use base_batcher_core::ThrottleClient;
use base_common_rpc_jsonrpsee::MinerApiExtClient;
use futures::future::BoxFuture;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

/// Production [`ThrottleClient`] that calls `miner_setMaxDASize` on the L2
/// execution node via the typed [`MinerApiExtClient`].
///
/// Connects to the standard (unauthenticated) HTTP RPC port. The `miner`
/// namespace must be enabled on the target node (`--http.api=...,miner`).
#[derive(Debug)]
pub struct RpcThrottleClient {
    client: HttpClient,
}

impl RpcThrottleClient {
    /// Build an [`RpcThrottleClient`] targeting `url`.
    ///
    /// Construction is synchronous — no connection is established until the
    /// first RPC call is made.
    pub fn new(url: &str) -> eyre::Result<Self> {
        let client = HttpClientBuilder::default().build(url)?;
        Ok(Self { client })
    }
}

impl ThrottleClient for RpcThrottleClient {
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async move {
            let success = self
                .client
                .set_max_da_size(U64::from(max_tx_size), U64::from(max_block_size))
                .await?;
            if !success {
                return Err("miner_setMaxDASize returned false".into());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;

    use super::*;

    fn json_rpc_response(result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":0,"result":{result}}}"#)
    }

    #[tokio::test]
    async fn set_max_da_size_sends_correct_request() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"miner_setMaxDASize"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(json_rpc_response("true"));
            })
            .await;

        let client = RpcThrottleClient::new(&server.url("/")).unwrap();
        client.set_max_da_size(150, 20_000).await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn set_max_da_size_false_return_is_error() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(json_rpc_response("false"));
            })
            .await;

        let client = RpcThrottleClient::new(&server.url("/")).unwrap();
        assert!(
            client.set_max_da_size(150, 20_000).await.is_err(),
            "false response must be treated as an error"
        );
    }

    #[tokio::test]
    async fn set_max_da_size_transport_error_propagates() {
        // Port 1 has no listener — connection will fail.
        let client = RpcThrottleClient::new("http://127.0.0.1:1").unwrap();
        assert!(
            client.set_max_da_size(150, 20_000).await.is_err(),
            "connection failure must propagate as error"
        );
    }
}
