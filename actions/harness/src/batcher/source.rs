//! L1 head source that lets the harness wait for the driver.
//!
//! Hand-rolled rather than mocked: the driver polls it from its `select!` loop for the whole
//! test, and the harness must interleave sync markers with the heads it feeds in FIFO order.

use async_trait::async_trait;
use base_batcher_source::L1HeadSource;
use tokio::sync::{mpsc, oneshot};

/// One item queued for the driver's L1 head source.
#[derive(Debug)]
pub enum L1HeadItem {
    /// Delivered to the driver as is.
    Head(u64),
    /// Answered when the driver is idle: the L1 head arm is the lowest-priority input of
    /// the driver's biased `select!` and is disabled while encoding is in progress, so it is
    /// only polled once the buffered work is encoded and submitted and every other input
    /// is waiting.
    Marker(oneshot::Sender<()>),
}

/// [`L1HeadSource`] fed by the harness through a channel.
#[derive(Debug)]
pub struct HarnessL1HeadSource {
    rx: mpsc::UnboundedReceiver<L1HeadItem>,
}

impl HarnessL1HeadSource {
    /// Create a source and the sender that feeds it.
    pub fn new() -> (Self, mpsc::UnboundedSender<L1HeadItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl L1HeadSource for HarnessL1HeadSource {
    async fn next(&mut self) -> u64 {
        loop {
            match self.rx.recv().await {
                Some(L1HeadItem::Head(head)) => return head,
                Some(L1HeadItem::Marker(reached)) => {
                    let _ = reached.send(());
                }
                None => std::future::pending().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn marker_is_answered_once_the_heads_before_it_are_taken() {
        let (mut source, tx) = HarnessL1HeadSource::new();
        let (reached_tx, mut reached_rx) = oneshot::channel();
        tx.send(L1HeadItem::Head(1)).unwrap();
        tx.send(L1HeadItem::Marker(reached_tx)).unwrap();
        tx.send(L1HeadItem::Head(2)).unwrap();

        assert_eq!(source.next().await, 1);
        assert!(reached_rx.try_recv().is_err(), "the marker must wait for the next poll");

        assert_eq!(source.next().await, 2);
        assert!(reached_rx.try_recv().is_ok(), "the next poll must answer the marker");
    }

    #[tokio::test]
    async fn marker_is_answered_even_when_nothing_follows() {
        let (mut source, tx) = HarnessL1HeadSource::new();
        let (reached_tx, reached_rx) = oneshot::channel();
        tx.send(L1HeadItem::Marker(reached_tx)).unwrap();

        // The source keeps waiting for a head, but the marker is already answered.
        let next = tokio::time::timeout(Duration::from_millis(10), source.next());
        assert!(next.await.is_err(), "no head must be returned");
        assert!(reached_rx.await.is_ok());
    }

    #[tokio::test]
    async fn parks_once_the_harness_is_gone() {
        let (mut source, tx) = HarnessL1HeadSource::new();
        drop(tx);

        let next = tokio::time::timeout(Duration::from_millis(10), source.next());
        assert!(next.await.is_err(), "a closed source must park");
    }
}
