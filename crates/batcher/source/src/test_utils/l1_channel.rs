//! Channel-backed [`L1HeadSource`] for tests.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::L1HeadSource;

/// An [`L1HeadSource`] backed by a `tokio::sync::mpsc` unbounded channel.
///
/// Use [`ChannelL1HeadSource::new`] to obtain a `(source, sender)` pair.
/// Head numbers sent on the [`mpsc::UnboundedSender`] side are consumed by
/// [`L1HeadSource::next`]. Once all senders are dropped, `next` parks forever.
#[derive(Debug)]
pub struct ChannelL1HeadSource {
    rx: mpsc::UnboundedReceiver<u64>,
}

impl ChannelL1HeadSource {
    /// Create a new channel L1 head source and its corresponding sender handle.
    pub fn new() -> (Self, mpsc::UnboundedSender<u64>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl L1HeadSource for ChannelL1HeadSource {
    async fn next(&mut self) -> u64 {
        match self.rx.recv().await {
            Some(head) => head,
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::*;

    #[tokio::test]
    async fn receive_l1_head() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        tx.send(42).unwrap();

        assert_eq!(source.next().await, 42);
    }

    #[tokio::test]
    async fn parks_when_empty_then_receives() {
        let (mut source, tx) = ChannelL1HeadSource::new();

        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            tx.send(99).unwrap();
        });

        assert_eq!(source.next().await, 99);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn fifo_ordering() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();

        assert_eq!(source.next().await, 1);
        assert_eq!(source.next().await, 2);
        assert_eq!(source.next().await, 3);
    }

    #[tokio::test]
    async fn parks_once_all_senders_are_dropped() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        drop(tx);

        assert!(source.next().now_or_never().is_none(), "a closed source must park");
    }
}
