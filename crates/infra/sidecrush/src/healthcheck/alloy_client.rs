use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::BlockId;
use async_trait::async_trait;

use super::{EthClient, HeaderSummary};

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
