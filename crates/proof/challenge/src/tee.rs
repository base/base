//! L1 head provider abstraction for fetching L1 block information.

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;

/// Trait for fetching L1 block information.
#[async_trait]
pub trait L1HeadProvider: Send + Sync + std::fmt::Debug {
    /// Returns the block number for the given L1 block hash.
    async fn block_number_by_hash(&self, hash: B256) -> eyre::Result<u64>;
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
    async fn block_number_by_hash(&self, hash: B256) -> eyre::Result<u64> {
        let block = self
            .provider
            .get_block_by_hash(hash)
            .await?
            .ok_or_else(|| eyre::eyre!("L1 block not found for hash {hash}"))?;
        Ok(block.header.number)
    }
}
