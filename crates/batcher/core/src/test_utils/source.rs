//! Test [`UnsafeBlockSource`] and [`L1HeadSource`] implementations.

use async_trait::async_trait;
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};

/// [`UnsafeBlockSource`] that parks the select arm forever.
///
/// Use this in tests that do not exercise the block-delivery path, so that
/// the driver's source arm never fires and other arms (receipts, L1 head,
/// safe-head watch) can be tested in isolation.
#[derive(Debug)]
pub struct PendingSource;

#[async_trait]
impl UnsafeBlockSource for PendingSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        std::future::pending().await
    }
}

/// [`UnsafeBlockSource`] that delivers exactly one default block then parks forever.
///
/// Useful for tests that need a single block ingestion event without the source
/// signalling exhaustion or causing a shutdown.
#[derive(Debug)]
pub struct OneBlockSource {
    delivered: bool,
}

impl OneBlockSource {
    /// Create a new source that has not yet delivered its block.
    pub const fn new() -> Self {
        Self { delivered: false }
    }
}

impl Default for OneBlockSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UnsafeBlockSource for OneBlockSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        if !self.delivered {
            self.delivered = true;
            Ok(L2BlockEvent::Block(Box::default()))
        } else {
            std::future::pending().await
        }
    }
}

/// [`L1HeadSource`] that parks the select arm forever.
///
/// Use this as the default L1 head source in driver tests that do not exercise
/// L1 head advancement, so that only other select arms fire.
#[derive(Debug)]
pub struct PendingL1HeadSource;

#[async_trait]
impl L1HeadSource for PendingL1HeadSource {
    async fn next(&mut self) -> Result<L1HeadEvent, SourceError> {
        std::future::pending().await
    }
}
