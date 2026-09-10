use std::ops::RangeInclusive;

use alloy_primitives::BlockNumber;
use futures::Stream;

use crate::BlockResponse;
use crate::DownloadResult;

/// Body downloader return type.
pub type BodyDownloaderResult = DownloadResult<Vec<BlockResponse>>;

/// A downloader capable of fetching and yielding block bodies from block headers.
///
/// A downloader represents a distinct strategy for submitting requests to download block bodies,
/// while a [`BodiesClient`][base_execution_network_wire::BodiesClient] represents a client capable of
/// fulfilling these requests.
pub trait BodyDownloader: Send + Stream<Item = BodyDownloaderResult> + Unpin {
    /// Method for setting the download range.
    fn set_download_range(&mut self, range: RangeInclusive<BlockNumber>) -> DownloadResult<()>;
}
