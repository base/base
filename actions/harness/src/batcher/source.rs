//! Block source that lets the harness wait for the driver.
//!
//! Hand-rolled rather than mocked: the driver polls it from its `select!` loop for the whole
//! test, and the harness must interleave sync markers with the events it feeds in FIFO order.

use async_trait::async_trait;
use base_batcher_source::{L2BlockEvent, SourceError, UnsafeBlockSource};
use tokio::sync::{mpsc, oneshot};

/// One item queued for the driver's block source.
#[derive(Debug)]
pub enum BlockSourceItem {
    /// Delivered to the driver as is.
    Event(L2BlockEvent),
    /// Answered when the driver waits on the source again, that is once it has taken every
    /// item queued before this one.
    ///
    /// [`BatchDriver::run`] does all the work it can before waiting for the next event, so by
    /// then the last of those items has been encoded and any frames it released submitted.
    ///
    /// [`BatchDriver::run`]: base_batcher_core::BatchDriver::run
    Marker(oneshot::Sender<()>),
}

/// [`UnsafeBlockSource`] fed by the harness through a channel.
#[derive(Debug)]
pub struct HarnessBlockSource {
    rx: mpsc::UnboundedReceiver<BlockSourceItem>,
}

impl HarnessBlockSource {
    /// Create a source and the sender that feeds it.
    pub fn new() -> (Self, mpsc::UnboundedSender<BlockSourceItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl UnsafeBlockSource for HarnessBlockSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        loop {
            match self.rx.recv().await.ok_or(SourceError::Closed)? {
                BlockSourceItem::Event(event) => return Ok(event),
                BlockSourceItem::Marker(reached) => {
                    let _ = reached.send(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn marker_is_answered_once_the_events_before_it_are_taken() {
        let (mut source, tx) = HarnessBlockSource::new();
        let (reached_tx, mut reached_rx) = oneshot::channel();
        tx.send(BlockSourceItem::Event(L2BlockEvent::Reorg)).unwrap();
        tx.send(BlockSourceItem::Marker(reached_tx)).unwrap();
        tx.send(BlockSourceItem::Event(L2BlockEvent::Reorg)).unwrap();

        assert!(matches!(source.next().await, Ok(L2BlockEvent::Reorg)));
        assert!(reached_rx.try_recv().is_err(), "the marker must wait for the next poll");

        assert!(matches!(source.next().await, Ok(L2BlockEvent::Reorg)));
        assert!(reached_rx.try_recv().is_ok(), "the next poll must answer the marker");
    }

    #[tokio::test]
    async fn marker_is_answered_even_when_nothing_follows() {
        let (mut source, tx) = HarnessBlockSource::new();
        let (reached_tx, reached_rx) = oneshot::channel();
        tx.send(BlockSourceItem::Marker(reached_tx)).unwrap();

        // The source keeps waiting for an event, but the marker is already answered.
        let next = tokio::time::timeout(Duration::from_millis(10), source.next());
        assert!(next.await.is_err(), "no event must be returned");
        assert!(reached_rx.await.is_ok());
    }

    #[tokio::test]
    async fn closed_channel_closes_the_source() {
        let (mut source, tx) = HarnessBlockSource::new();
        drop(tx);

        assert!(matches!(source.next().await, Err(SourceError::Closed)));
    }
}
