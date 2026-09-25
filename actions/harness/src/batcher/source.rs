//! L1 head source that lets the harness wait for the driver.
//!
//! Hand-rolled rather than mocked: the driver polls it from its `select!` loop for the whole
//! test, and the harness must interleave sync markers with the heads it feeds in FIFO order.

use std::task::Poll;

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
    ///
    /// The source answers a marker on the poll after the one that takes it, so the driver
    /// has checked every other input again since the marker was queued: what the harness
    /// sent just before the marker is then taken first, even on a multi-thread runtime.
    Marker(oneshot::Sender<()>),
}

/// [`L1HeadSource`] fed by the harness through a channel.
#[derive(Debug)]
pub struct HarnessL1HeadSource {
    /// Heads and markers queued by the harness, in order.
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
                    // Return pending once, with the task woken so the driver polls again at
                    // once, and answer on that next poll.
                    let mut polled = false;
                    std::future::poll_fn(|cx| {
                        if polled {
                            return Poll::Ready(());
                        }
                        polled = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    })
                    .await;
                    let _ = reached.send(());
                }
                None => std::future::pending().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        task::{Context, Waker},
        time::Duration,
    };

    use super::*;

    #[tokio::test]
    async fn marker_is_answered_by_the_poll_after_the_one_that_takes_it() {
        let (mut source, tx) = HarnessL1HeadSource::new();
        let (reached_tx, mut reached_rx) = oneshot::channel();
        tx.send(L1HeadItem::Head(1)).unwrap();
        tx.send(L1HeadItem::Marker(reached_tx)).unwrap();

        assert_eq!(source.next().await, 1);
        assert!(reached_rx.try_recv().is_err(), "the marker must wait for the next poll");

        let mut next = pin!(source.next());
        assert!(next.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(
            reached_rx.try_recv().is_err(),
            "the poll that takes the marker must not answer it"
        );

        assert!(next.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(reached_rx.try_recv().is_ok(), "the poll after must answer the marker");
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
