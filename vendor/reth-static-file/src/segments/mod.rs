//! `StaticFile` segment implementations and utilities.

mod receipts;
use std::ops::RangeInclusive;

use alloy_primitives::BlockNumber;
pub use receipts::Receipts;
use reth_provider::StaticFileProviderFactory;
use reth_static_file_types::StaticFileSegment;
use reth_storage_errors::provider::ProviderResult;

/// A segment represents moving some portion of the data to static files.
pub trait Segment<Provider: StaticFileProviderFactory>: Send + Sync {
    /// Returns the [`StaticFileSegment`].
    fn segment(&self) -> StaticFileSegment;

    /// Move data to static files for the provided block range.
    /// [`StaticFileProvider`](reth_provider::providers::StaticFileProvider) will handle
    /// the management of and writing to files.
    fn copy_to_static_files(
        &self,
        provider: Provider,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<()>;
}
