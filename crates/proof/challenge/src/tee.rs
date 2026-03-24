//! L1 head provider abstraction for fetching the finalized L1 block hash.

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use async_trait::async_trait;

/// Trait for fetching the L1 finalized head.
#[async_trait]
pub trait L1HeadProvider: Send + Sync + std::fmt::Debug {
    /// Returns the hash and block number of the current L1 finalized block.
    async fn finalized_head(&self) -> eyre::Result<(B256, u64)>;
}

/// [`L1HeadProvider`] backed by an RPC [`RootProvider`].
#[derive(Debug)]
pub struct RpcL1HeadProvider {
    provider: RootProvider,
}

impl RpcL1HeadProvider {
    /// Creates a new provider wrapping the given RPC root provider.
    pub const fn new(provider: RootProvider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl L1HeadProvider for RpcL1HeadProvider {
    async fn finalized_head(&self) -> eyre::Result<(B256, u64)> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Finalized)
            .await?
            .ok_or_else(|| eyre::eyre!("L1 finalized block not found"))?;
        Ok((block.header.hash, block.header.number))
    }
}
