use alloy_provider::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy_pubsub::{ConnectionHandle, PubSubConnect};
use alloy_rpc_types_eth::BlockId;
use alloy_transport::{TransportErrorKind, TransportResult};
use async_trait::async_trait;
use futures::{StreamExt, stream};

use super::{EthClient, HeadStream, HeadSubscriber, HeaderSummary};

/// An [`EthClient`] backed by an [`alloy_provider::Provider`] HTTP transport.
#[derive(Debug, Clone)]
pub struct AlloyEthClient {
    provider: RootProvider,
}

impl AlloyEthClient {
    /// Construct a client that talks to an execution-layer node over HTTP JSON-RPC.
    ///
    /// The provider is built with `disable_recommended_fillers()` because sidecrush only issues
    /// read-only `eth_getBlockByNumber` calls and never needs nonce/gas-price/chain-id fillers.
    pub fn new_http(url: &str) -> anyhow::Result<Self> {
        let provider =
            ProviderBuilder::new().disable_recommended_fillers().connect_http(url.parse()?);
        Ok(Self { provider })
    }
}

#[async_trait]
impl EthClient for AlloyEthClient {
    async fn latest_header(
        &self,
    ) -> Result<HeaderSummary, Box<dyn std::error::Error + Send + Sync>> {
        let block = self
            .provider
            .get_block(BlockId::latest())
            .hashes()
            .await?
            .ok_or_else(|| "latest block not found".to_string())?;

        let number: u64 = block.header.number;
        let timestamp_unix_seconds: u64 = block.header.timestamp;
        let transaction_count: usize = block.transactions.len();

        Ok(HeaderSummary { number, timestamp_unix_seconds, transaction_count })
    }
}

/// [`HeadSubscriber`] backed by an `alloy` WebSocket provider and `eth_subscribe("newHeads")`.
///
/// Headers from `newHeads` carry no transactions, so [`HeaderSummary::transaction_count`] is
/// always 0 for heads from this subscriber.
#[derive(Debug, Clone)]
pub struct AlloyWsHeadSubscriber {
    url: String,
}

impl AlloyWsHeadSubscriber {
    /// Create a subscriber for a `ws://` or `wss://` endpoint. Nothing connects until
    /// [`HeadSubscriber::subscribe`] is called.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

/// A [`WsConnect`] that never reconnects internally.
///
/// Alloy's retry limit only bounds *failed* internal reconnects: on a dropped socket the
/// pubsub service still transparently reconnects and re-issues subscriptions, hiding the
/// outage from the checker (worst case it stays hidden while a replacement handshake hangs).
/// This connector keeps only the initial connect. Any internal reconnect attempt fails with
/// a non-retryable error, which shuts the pubsub service down and ends the head stream, so
/// the checker's own classification and backoff stay authoritative.
#[derive(Debug, Clone)]
struct SingleConnectWs(WsConnect);

impl PubSubConnect for SingleConnectWs {
    fn is_local(&self) -> bool {
        PubSubConnect::is_local(&self.0)
    }

    async fn connect(&self) -> TransportResult<ConnectionHandle> {
        PubSubConnect::connect(&self.0).await
    }

    async fn try_reconnect(&self) -> TransportResult<ConnectionHandle> {
        Err(TransportErrorKind::non_retryable_str("internal reconnects are disabled"))
    }
}

#[async_trait]
impl HeadSubscriber for AlloyWsHeadSubscriber {
    async fn subscribe(&self) -> Result<HeadStream, Box<dyn std::error::Error + Send + Sync>> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_pubsub_with(SingleConnectWs(WsConnect::new(self.url.as_str())))
            .await?;
        let subscription = provider.subscribe_blocks().await?;
        // The subscription only delivers heads while the provider's pubsub backend is alive,
        // so the stream owns the provider; dropping the stream closes the connection.
        let heads = stream::unfold(
            (provider, subscription.into_stream()),
            |(provider, mut headers)| async move {
                let header = headers.next().await?;
                let summary = HeaderSummary {
                    number: header.number,
                    timestamp_unix_seconds: header.timestamp,
                    transaction_count: 0,
                };
                Some((summary, (provider, headers)))
            },
        );
        Ok(Box::pin(heads))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    /// Serve one WebSocket connection: complete the handshake, answer `eth_subscribe`, then
    /// close the socket. The next connection is accepted but never completes the handshake,
    /// so any internal reconnect attempt hangs instead of succeeding.
    async fn spawn_handshake_then_stall_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(conn).await.unwrap();
            let request = ws.next().await.unwrap().unwrap();
            let request: serde_json::Value =
                serde_json::from_str(request.to_text().unwrap()).unwrap();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": "0xdeadbeef",
            });
            ws.send(Message::text(response.to_string())).await.unwrap();
            // Abruptly close the established connection, as a node restart would.
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(ws);

            // Hold a TCP connection open without a WebSocket handshake. It can only be
            // reached by an internal reconnect, which must never be attempted.
            let (_held, _) = listener.accept().await.unwrap();
            // Outlives the test's 10 s stream timeout; the runtime drops it at test end.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn closed_socket_ends_subscription_instead_of_internal_reconnect() {
        let url = spawn_handshake_then_stall_server().await;
        let subscriber = AlloyWsHeadSubscriber::new(url);

        let mut stream = subscriber.subscribe().await.expect("subscribe should succeed");

        // The server closed the connection right after the subscription was confirmed, and
        // its replacement connection never completes the handshake. The stream must end so
        // the checker reports `Error` and reconnects with its own backoff; a pending stream
        // would mean alloy silently reconnected internally.
        let ended = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        assert!(matches!(ended, Ok(None)), "head stream should end, got {ended:?}");
    }
}
