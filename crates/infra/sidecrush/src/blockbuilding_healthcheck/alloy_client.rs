use super::{EthClient, HeaderSummary};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::BlockId;
use async_trait::async_trait;

#[derive(Clone)]
pub struct AlloyEthClient {
    provider: RootProvider,
}

impl AlloyEthClient {
    pub fn new_http(url: &str) -> anyhow::Result<Self> {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(url.parse()?);
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

        Ok(HeaderSummary {
            number,
            timestamp_unix_seconds,
        })
    }
}
