use std::{fmt::Debug, ops::RangeInclusive};

use alloy_primitives::BlockNumber;
use base_execution_network_service::{
    BlockResponse, BodyDownloader, DownloadError, DownloadResult,
};
use futures::Stream;

/// A [`BodyDownloader`] implementation that does nothing.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct NoopBodiesDownloader<B> {
    _block: std::marker::PhantomData<B>,
}

impl BodyDownloader for NoopBodiesDownloader<base_common_types_chain::BaseBlock> {
    fn set_download_range(&mut self, _: RangeInclusive<BlockNumber>) -> DownloadResult<()> {
        Ok(())
    }
}

impl Stream for NoopBodiesDownloader<base_common_types_chain::BaseBlock> {
    type Item = Result<Vec<BlockResponse>, DownloadError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        panic!("NoopBodiesDownloader shouldn't be polled.")
    }
}
