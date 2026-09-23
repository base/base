//! Test [`UnsafeBlockSource`] and [`L1HeadSource`] implementations.

use std::sync::{Arc, Mutex};

use alloy_consensus::Header;
use async_trait::async_trait;
use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;

/// [`UnsafeBlockSource`] that parks the select arm forever.
///
/// Use this in tests that do not exercise the block-delivery path, so that
/// the driver's source arm never fires and other arms (receipts, L1 head,
/// derivation-status feed) can be tested in isolation.
#[derive(Debug)]
pub struct PendingSource;

#[async_trait]
impl UnsafeBlockSource for PendingSource {
    async fn next(&mut self) -> L2BlockEvent {
        std::future::pending().await
    }
}

/// [`UnsafeBlockSource`] that records sequential catchup requests and otherwise parks.
#[derive(Debug)]
pub struct TrackingSource {
    catchup_heads: Arc<Mutex<Vec<BlockInfo>>>,
}

impl TrackingSource {
    /// Create a source and its shared catchup call log.
    pub fn new() -> (Self, Arc<Mutex<Vec<BlockInfo>>>) {
        let catchup_heads = Arc::new(Mutex::new(Vec::new()));
        (Self { catchup_heads: Arc::clone(&catchup_heads) }, catchup_heads)
    }
}

#[async_trait]
impl UnsafeBlockSource for TrackingSource {
    async fn next(&mut self) -> L2BlockEvent {
        std::future::pending().await
    }

    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.catchup_heads.lock().unwrap().push(safe_head);
    }
}

/// [`UnsafeBlockSource`] that delivers exactly one block, numbered 1, then parks forever.
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
    async fn next(&mut self) -> L2BlockEvent {
        if !self.delivered {
            self.delivered = true;
            L2BlockEvent::Block(Box::new(BaseBlock {
                header: Header { number: 1, ..Default::default() },
                body: Default::default(),
            }))
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
    async fn next(&mut self) -> u64 {
        std::future::pending().await
    }
}
