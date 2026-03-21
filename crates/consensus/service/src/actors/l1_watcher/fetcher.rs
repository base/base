//! Narrow provider trait for [`super::L1WatcherActor`].

use alloy_eips::BlockId;
use alloy_rpc_types_eth::{Block, Filter, Log};
use async_trait::async_trait;

/// A narrow trait exposing only the two L1 RPC methods used by [`super::L1WatcherActor`].
///
/// Replacing the broad [`alloy_provider::Provider`] bound with this trait makes
/// in-process test implementations straightforward — a test double only needs
/// to implement `get_logs` and `get_block` rather than the full ~30-method
/// provider interface.
#[async_trait]
pub trait L1BlockFetcher: Send + Sync + 'static {
    /// Error type returned by all fetch operations.
    type Error: std::fmt::Display + std::fmt::Debug + Send;

    /// Return all logs matching `filter`.
    async fn get_logs(&self, filter: Filter) -> Result<Vec<Log>, Self::Error>;

    /// Return the block identified by `id`, or `None` if it does not exist.
    async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error>;
}

/// Wraps an [`alloy_provider::Provider`] to implement [`L1BlockFetcher`].
///
/// Construct this with the production L1 provider and pass it to
/// [`super::L1WatcherActor::new`] in place of the bare provider.
#[derive(Debug)]
pub struct AlloyL1BlockFetcher<P>(pub P);

#[async_trait]
impl<P> L1BlockFetcher for AlloyL1BlockFetcher<P>
where
    P: alloy_provider::Provider + 'static,
{
    type Error = alloy_transport::TransportError;

    async fn get_logs(&self, filter: Filter) -> Result<Vec<Log>, Self::Error> {
        Ok(self.0.get_logs(&filter).await?)
    }

    async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error> {
        self.0.get_block(id).await
    }
}
