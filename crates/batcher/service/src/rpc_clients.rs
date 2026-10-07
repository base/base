//! Builds the JSON-RPC clients of the batcher.

use std::time::Duration;

use alloy_provider::{RootProvider, network::Network};
use alloy_rpc_client::RpcClient;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use url::Url;

/// Builds the JSON-RPC clients of the batcher.
///
/// Every request of every client fails once the network timeout elapses, so an endpoint that
/// stops answering never holds the batcher.
#[derive(Debug, Clone, Copy)]
pub struct RpcClients {
    network_timeout: Duration,
}

impl RpcClients {
    /// Creates a builder of clients whose requests time out after `network_timeout`.
    pub const fn new(network_timeout: Duration) -> Self {
        Self { network_timeout }
    }

    /// An alloy provider of the HTTP endpoint at `url`.
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is not an HTTP URL, or when reqwest cannot initialize its
    /// TLS backend.
    pub fn provider<N: Network>(&self, url: &Url) -> eyre::Result<RootProvider<N>> {
        let origin = url.origin().ascii_serialization();
        if !matches!(url.scheme(), "http" | "https") {
            eyre::bail!("{origin} is not an HTTP URL");
        }
        let http = reqwest::Client::builder()
            .timeout(self.network_timeout)
            .build()
            .map_err(|e| eyre::eyre!("failed to build the provider for {origin}: {e}"))?;
        Ok(RootProvider::new(RpcClient::new_http_with_client(http, url.clone())))
    }

    /// A jsonrpsee client of the HTTP endpoint at `url`.
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is not an HTTP URL.
    pub fn client(&self, url: &Url) -> eyre::Result<HttpClient> {
        HttpClientBuilder::default()
            .request_timeout(self.network_timeout)
            .build(url.as_str())
            .map_err(|e| {
                let origin = url.origin().ascii_serialization();
                eyre::eyre!("failed to build the client for {origin}: {e}")
            })
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use alloy_provider::{Provider, network::Ethereum};
    use jsonrpsee::{
        core::{ClientError, client::ClientT},
        rpc_params,
    };

    use super::*;

    const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

    /// The URL of an endpoint that accepts connections but never answers, and the listener
    /// that keeps it open.
    fn silent_endpoint() -> (Url, TcpListener) {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", silent.local_addr().unwrap()).parse().unwrap();
        (url, silent)
    }

    /// A request through a provider of an endpoint that accepts the connection but never
    /// answers fails once the network timeout elapses.
    #[tokio::test(start_paused = true)]
    async fn a_provider_request_fails_when_the_endpoint_never_answers() {
        let (silent_url, _silent) = silent_endpoint();
        let provider: RootProvider =
            RpcClients::new(NETWORK_TIMEOUT).provider(&silent_url).unwrap();

        let error = tokio::time::timeout(NETWORK_TIMEOUT * 2, provider.get_chain_id())
            .await
            .expect("the request must fail instead of hanging")
            .unwrap_err();

        let timed_out = error
            .as_transport_err()
            .and_then(|error| error.as_custom())
            .and_then(|error| error.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
        assert!(timed_out, "{error}");
    }

    /// A request through a client of an endpoint that accepts the connection but never answers
    /// fails once the network timeout elapses.
    #[tokio::test(start_paused = true)]
    async fn a_client_request_fails_when_the_endpoint_never_answers() {
        let (silent_url, _silent) = silent_endpoint();
        let client = RpcClients::new(NETWORK_TIMEOUT).client(&silent_url).unwrap();

        let error = tokio::time::timeout(
            NETWORK_TIMEOUT * 2,
            client.request::<String, _>("eth_chainId", rpc_params![]),
        )
        .await
        .expect("the request must fail instead of hanging")
        .unwrap_err();

        assert!(matches!(error, ClientError::RequestTimeout), "{error}");
    }

    /// A provider is only built for an HTTP URL, so a wrong scheme fails at startup instead of
    /// on every request.
    #[test]
    fn provider_rejects_a_non_http_url() {
        let url: Url = "ws://127.0.0.1:1".parse().unwrap();

        assert!(RpcClients::new(NETWORK_TIMEOUT).provider::<Ethereum>(&url).is_err());
    }
}
