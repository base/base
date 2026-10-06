//! Test [`UnsafeBlockSource`] and [`L1HeadSource`] implementations.
//!
//! Hand-rolled rather than mocked because `next` parks forever, which `mockall` expectations
//! cannot express.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
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

/// [`UnsafeBlockSource`] that delivers its queued events, then parks, and records sequential
/// catchup requests.
#[derive(Debug)]
pub struct TrackingSource {
    events: VecDeque<L2BlockEvent>,
    catchup_heads: Arc<Mutex<Vec<BlockInfo>>>,
}

impl TrackingSource {
    /// Create a source with no events and its shared catchup call log.
    pub fn new() -> (Self, Arc<Mutex<Vec<BlockInfo>>>) {
        let catchup_heads = Arc::new(Mutex::new(Vec::new()));
        (Self { events: VecDeque::new(), catchup_heads: Arc::clone(&catchup_heads) }, catchup_heads)
    }

    /// Queue `events`, delivered in order before the source parks.
    pub fn with_events(mut self, events: impl IntoIterator<Item = L2BlockEvent>) -> Self {
        self.events.extend(events);
        self
    }
}

#[async_trait]
impl UnsafeBlockSource for TrackingSource {
    async fn next(&mut self) -> L2BlockEvent {
        match self.events.pop_front() {
            Some(event) => event,
            None => std::future::pending().await,
        }
    }

    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.catchup_heads.lock().unwrap().push(safe_head);
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
