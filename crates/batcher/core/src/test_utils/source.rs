//! Test [`UnsafeBlockSource`] and [`L1HeadSource`] implementations.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_batcher_source::{
    L1HeadSource, L2BlockEvent, UnsafeBlockSource, test_utils::ChannelBlockSource,
};
use base_protocol::BlockInfo;
use tokio::sync::mpsc;

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

/// [`UnsafeBlockSource`] fed by a channel, which records the safe heads the driver asks it
/// to catch up from.
///
/// Hand-rolled rather than mocked: `reset_catchup` is recorded while the driver runs.
#[derive(Debug)]
pub struct TrackingSource {
    events: ChannelBlockSource,
    catchup_heads: Arc<Mutex<Vec<BlockInfo>>>,
}

impl TrackingSource {
    /// Create a source, the sender that feeds it and its shared catch-up log. The source
    /// parks once the sender is dropped.
    pub fn new() -> (Self, mpsc::UnboundedSender<L2BlockEvent>, Arc<Mutex<Vec<BlockInfo>>>) {
        let (events, events_tx) = ChannelBlockSource::new();
        let catchup_heads = Arc::new(Mutex::new(Vec::new()));
        (Self { events, catchup_heads: Arc::clone(&catchup_heads) }, events_tx, catchup_heads)
    }
}

#[async_trait]
impl UnsafeBlockSource for TrackingSource {
    async fn next(&mut self) -> L2BlockEvent {
        self.events.next().await
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
