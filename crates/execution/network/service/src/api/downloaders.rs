//! API related to syncing blocks.

use base_execution_network_wire::{BlockAccessListsClient, BlockClient};
use futures::Future;
use tokio::sync::oneshot;

/// Provides client for downloading blocks.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockDownloaderProvider {
    /// The client this type can provide.
    type Client: BlockClient + BlockAccessListsClient + Send + Sync + Clone + 'static;

    /// Returns a new [`BlockClient`], used for fetching blocks from peers.
    ///
    /// The client is the entrypoint for sending block requests to the network.
    fn fetch_client(
        &self,
    ) -> impl Future<Output = Result<Self::Client, oneshot::error::RecvError>> + Send;
}
