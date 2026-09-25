//! Helpers for Base-specific RPC implementations.

use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_json_rpc::{RpcRecv, RpcSend};
use alloy_primitives::{B256, hex};
use alloy_rpc_client::{BuiltInConnectionString, ClientBuilder, RpcClient as Client};
use alloy_transport_http::{Http, reqwest as alloy_reqwest};
use thiserror::Error;
use tracing::warn;

use crate::{SequencerClientError, metrics::SequencerMetrics};

/// Upper bound for a single HTTP request to the sequencer, so a stalled endpoint can't hold a
/// forwarding call open indefinitely.
const SEQUENCER_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Sequencer client error
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid scheme
    #[error("Invalid scheme of sequencer url: {0}")]
    InvalidScheme(String),
    /// Invalid header or value provided.
    #[error("Invalid header: {0}")]
    InvalidHeader(String),
    /// Invalid url
    #[error("Invalid sequencer url: {0}")]
    InvalidUrl(String),
    /// Establishing a connection to the sequencer endpoint resulted in an error.
    #[error("Failed to connect to sequencer: {0}")]
    TransportError(
        #[from]
        #[source]
        alloy_transport::TransportError,
    ),
    /// Reqwest failed to init client
    #[error("Failed to init reqwest client for sequencer: {0}")]
    ReqwestError(
        #[from]
        #[source]
        alloy_transport_http::reqwest::Error,
    ),
}

/// A client to interact with a Sequencer
#[derive(Debug, Clone)]
pub struct SequencerClient {
    inner: Arc<SequencerClientInner>,
}

impl SequencerClientInner {
    /// Creates a new instance with the given endpoint and client.
    pub const fn new(sequencer_endpoint: String, client: Client) -> Self {
        Self { sequencer_endpoint, client }
    }
}

impl SequencerClient {
    /// Creates a new [`SequencerClient`] for the given URL.
    ///
    /// If the URL is a websocket endpoint we connect a websocket instance.
    pub async fn new(sequencer_endpoint: impl Into<String>) -> Result<Self, Error> {
        Self::new_with_headers(sequencer_endpoint, Default::default()).await
    }

    /// Creates a new `SequencerClient` for the given URL with the given headers
    ///
    /// This expects headers in the form: `header=value`
    pub async fn new_with_headers(
        sequencer_endpoint: impl Into<String>,
        headers: Vec<String>,
    ) -> Result<Self, Error> {
        let sequencer_endpoint = sequencer_endpoint.into();
        let endpoint = BuiltInConnectionString::from_str(&sequencer_endpoint)?;
        if matches!(&endpoint, BuiltInConnectionString::Http(_)) {
            Self::new_http_with_headers(sequencer_endpoint, headers)
        } else {
            let client = ClientBuilder::default().connect_with(endpoint).await?;
            let inner = SequencerClientInner::new(sequencer_endpoint, client);
            Ok(Self { inner: Arc::new(inner) })
        }
    }

    /// Creates an HTTP sequencer client synchronously, with optional `header=value` headers.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-HTTP URL, invalid headers, or HTTP client construction.
    pub fn new_http_with_headers(
        sequencer_endpoint: impl Into<String>,
        headers: Vec<String>,
    ) -> Result<Self, Error> {
        Self::new_http_with_headers_and_timeout(sequencer_endpoint, headers, SEQUENCER_HTTP_TIMEOUT)
    }

    fn new_http_with_headers_and_timeout(
        sequencer_endpoint: impl Into<String>,
        headers: Vec<String>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let sequencer_endpoint = sequencer_endpoint.into();
        if !matches!(
            BuiltInConnectionString::from_str(&sequencer_endpoint)?,
            BuiltInConnectionString::Http(_)
        ) {
            return Err(Error::InvalidScheme(sequencer_endpoint));
        }
        let mut builder = alloy_reqwest::Client::builder().use_rustls_tls().timeout(timeout);
        if !headers.is_empty() {
            let mut header_map = alloy_reqwest::header::HeaderMap::new();
            for header in headers {
                // Don't echo the entry itself, it usually carries a credential.
                let Some((key, value)) = header.split_once('=') else {
                    return Err(Error::InvalidHeader(
                        "expected `name=value`, found an entry without `=`".to_string(),
                    ));
                };
                header_map.insert(
                    key.trim()
                        .parse::<alloy_reqwest::header::HeaderName>()
                        .map_err(|err| Error::InvalidHeader(err.to_string()))?,
                    value
                        .trim()
                        .parse::<alloy_reqwest::header::HeaderValue>()
                        .map_err(|err| Error::InvalidHeader(err.to_string()))?,
                );
            }
            builder = builder.default_headers(header_map);
        }
        Self::with_http_client(sequencer_endpoint, builder.build()?)
    }

    /// Creates a new [`SequencerClient`] with http transport with the given http client.
    pub fn with_http_client(
        sequencer_endpoint: impl Into<String>,
        client: alloy_reqwest::Client,
    ) -> Result<Self, Error> {
        let sequencer_endpoint: String = sequencer_endpoint.into();
        let url = sequencer_endpoint
            .parse()
            .map_err(|_| Error::InvalidUrl(sequencer_endpoint.clone()))?;

        let http_client = Http::with_client(client, url);
        let is_local = http_client.guess_local();
        let client = ClientBuilder::default().transport(http_client, is_local);

        let inner = SequencerClientInner::new(sequencer_endpoint, client);
        Ok(Self { inner: Arc::new(inner) })
    }

    /// Returns the network of the client
    pub fn endpoint(&self) -> &str {
        &self.inner.sequencer_endpoint
    }

    /// Returns the client
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    /// Sends a [`alloy_rpc_client::RpcCall`] request to the sequencer endpoint.
    pub async fn request<Params: RpcSend, Resp: RpcRecv>(
        &self,
        method: &str,
        params: Params,
    ) -> Result<Resp, SequencerClientError> {
        let resp =
            self.client().request::<Params, Resp>(method.to_string(), params).await.inspect_err(
                |err| {
                    warn!(
                        target: "rpc::sequencer",
                        %err,
                        "HTTP request to sequencer failed",
                    );
                },
            )?;
        Ok(resp)
    }

    /// Forwards a transaction to the sequencer endpoint.
    pub async fn forward_raw_transaction(&self, tx: &[u8]) -> Result<B256, SequencerClientError> {
        let start = Instant::now();
        let rlp_hex = hex::encode_prefixed(tx);
        let tx_hash =
            self.request("eth_sendRawTransaction", (rlp_hex,)).await.inspect_err(|err| {
                warn!(
                    target: "rpc::eth",
                    %err,
                    "Failed to forward transaction to sequencer",
                );
            })?;
        SequencerMetrics::record_forward_latency(start.elapsed());
        Ok(tx_hash)
    }
}

/// Inner state of a [`SequencerClient`].
#[derive(Debug)]
pub struct SequencerClientInner {
    /// The endpoint of the sequencer
    pub sequencer_endpoint: String,
    /// The RPC client
    pub client: Client,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U64;

    use super::*;

    #[tokio::test]
    async fn test_http_body_str() {
        let client = SequencerClient::new("http://localhost:8545").await.unwrap();

        let request = client
            .client()
            .make_request("eth_getBlockByNumber", (U64::from(10),))
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_getBlockByNumber","params":["0xa"],"id":0,"jsonrpc":"2.0"}"#
        );
    }

    #[test]
    fn http_headers_require_name_value_form() {
        let err = SequencerClient::new_http_with_headers(
            "http://localhost:8545",
            vec!["Authorization: Bearer secret-token".to_string()],
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidHeader(_)));
        assert!(!err.to_string().contains("secret-token"));

        SequencerClient::new_http_with_headers(
            "http://localhost:8545",
            vec!["Authorization=Bearer secret-token".to_string()],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn http_request_times_out_on_stalled_sequencer() {
        // Accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });

        let client = SequencerClient::new_http_with_headers_and_timeout(
            format!("http://{addr}"),
            vec![],
            Duration::from_millis(200),
        )
        .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.request::<_, U64>("eth_chainId", ()),
        )
        .await
        .expect("request should fail on its own timeout, not hang");
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore = "Start if WS is reachable at ws://localhost:8546"]
    async fn test_ws_body_str() {
        let client = SequencerClient::new("ws://localhost:8546").await.unwrap();

        let request = client
            .client()
            .make_request("eth_getBlockByNumber", (U64::from(10),))
            .serialize()
            .unwrap()
            .take_request();
        let body = request.get();

        assert_eq!(
            body,
            r#"{"method":"eth_getBlockByNumber","params":["0xa"],"id":0,"jsonrpc":"2.0"}"#
        );
    }
}
