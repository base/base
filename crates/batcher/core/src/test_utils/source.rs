//! Test [`UnsafeBlockSource`] and [`L1HeadSource`] implementations.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};

/// [`UnsafeBlockSource`] that parks the select arm forever but records every
/// [`reset_catchup`](UnsafeBlockSource::reset_catchup) call.
///
/// Use this to assert the block number the driver restarts sequential catchup
/// from after a reorg or a safe-head regression.
#[derive(Debug, Clone, Default)]
pub struct RecordingSource {
    /// `start_from` values passed to `reset_catchup`, in call order.
    pub catchups: Arc<Mutex<Vec<u64>>>,
}

impl RecordingSource {
    /// Create a new recording source backed by the given shared vector.
    pub const fn new(catchups: Arc<Mutex<Vec<u64>>>) -> Self {
        Self { catchups }
    }
}

#[async_trait]
impl UnsafeBlockSource for RecordingSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        std::future::pending().await
    }

    fn reset_catchup(&mut self, start_from: u64) {
        self.catchups.lock().unwrap().push(start_from);
    }
}

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
