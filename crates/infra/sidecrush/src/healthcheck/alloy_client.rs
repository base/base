use alloy_provider::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy_rpc_types_eth::BlockId;
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

#[async_trait]
impl HeadSubscriber for AlloyWsHeadSubscriber {
    async fn subscribe(&self) -> Result<HeadStream, Box<dyn std::error::Error + Send + Sync>> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_ws(WsConnect::new(self.url.as_str()))
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
