use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use alloy_primitives::BlockNumber;
use base_common_consensus::BlockHeader;
use base_execution_consensus::BaseBeaconConsensus;
use futures::{Stream, stream::FuturesUnordered};
use futures_util::StreamExt;
use reth_network_p2p::{
    bodies::{client::BodiesClient, response::BlockResponse},
    error::DownloadResult,
};
use reth_primitives_traits::SealedHeader;

use super::request::BodiesRequestFuture;
use crate::metrics::BodyDownloaderMetrics;

/// The wrapper around [`FuturesUnordered`] that keeps information
/// about the blocks currently being requested.
#[derive(Debug)]
pub(crate) struct BodiesRequestQueue<C: BodiesClient<Body = base_common_consensus::BaseBlockBody>> {
    /// Inner body request queue.
    inner: FuturesUnordered<BodiesRequestFuture<C>>,
    /// The downloader metrics.
    metrics: BodyDownloaderMetrics,
    /// Last requested block number.
    pub(crate) last_requested_block_number: Option<BlockNumber>,
}

impl<C> BodiesRequestQueue<C>
where
    C: BodiesClient<Body = base_common_consensus::BaseBlockBody> + 'static,
{
    /// Create new instance of request queue.
    pub(crate) fn new(metrics: BodyDownloaderMetrics) -> Self {
        Self { metrics, inner: Default::default(), last_requested_block_number: None }
    }

    /// Returns `true` if the queue is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of queued requests.
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Clears the inner queue and related data.
    pub(crate) fn clear(&mut self) {
        self.inner.clear();
        self.last_requested_block_number.take();
        self.metrics.clear();
    }
    /// Add new request to the queue.
    /// Expects a sorted list of headers.
    pub(crate) fn push_new_request(
        &mut self,
        client: Arc<C>,
        consensus: Arc<BaseBeaconConsensus>,
        request: Vec<SealedHeader>,
    ) {
        // Set last max requested block number
        self.last_requested_block_number = request
            .last()
            .map(|last| match self.last_requested_block_number {
                Some(num) => last.number().max(num),
                None => last.number(),
            })
            .or(self.last_requested_block_number);

        // Create request and push into the queue.
        self.inner.push(
            BodiesRequestFuture::new(client, consensus, self.metrics.clone()).with_headers(request),
        )
    }
}

impl<C> Stream for BodiesRequestQueue<C>
where
    C: BodiesClient<Body = base_common_consensus::BaseBlockBody> + 'static,
{
    type Item = DownloadResult<Vec<BlockResponse>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().inner.poll_next_unpin(cx)
    }
}
