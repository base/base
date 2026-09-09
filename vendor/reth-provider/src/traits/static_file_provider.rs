use alloy_primitives::BlockNumber;
use base_execution_state_types::StaticFileSegment;
use reth_storage_errors::provider::ProviderResult;

use crate::providers::{StaticFileProvider, StaticFileProviderRWRefMut};

/// Static file provider factory.
pub trait StaticFileProviderFactory {
    /// Create new instance of static file provider.
    fn static_file_provider(&self) -> StaticFileProvider;

    /// Returns a mutable reference to a
    /// [`StaticFileProviderRW`](`crate::providers::StaticFileProviderRW`) of a
    /// [`StaticFileSegment`].
    fn get_static_file_writer(
        &self,
        block: BlockNumber,
        segment: StaticFileSegment,
    ) -> ProviderResult<StaticFileProviderRWRefMut<'_>>;
}
