//! L1 head provider abstraction for resolving block numbers from hashes.

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_rpc::L1Provider;

/// Trait for resolving an L1 block number from its hash.
#[async_trait]
pub trait L1HeadProvider: Send + Sync + std::fmt::Debug {
    /// Returns the block number for the given L1 block hash.
    async fn block_number_by_hash(&self, hash: B256) -> eyre::Result<u64>;
}

/// Blanket implementation: any [`L1Provider`] that is also [`Debug`] can serve
/// as an [`L1HeadProvider`] by delegating to [`L1Provider::header_by_hash`].
#[async_trait]
impl<T: L1Provider + std::fmt::Debug> L1HeadProvider for T {
    async fn block_number_by_hash(&self, hash: B256) -> eyre::Result<u64> {
        let header = self.header_by_hash(hash).await?;
        Ok(header.number)
    }
}
