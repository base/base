//! Channel-backed [`L1HeadSource`] for tests.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{L1HeadEvent, L1HeadSource, SourceError};

/// An [`L1HeadSource`] backed by a `tokio::sync::mpsc` unbounded channel.
///
/// Use [`ChannelL1HeadSource::new`] to obtain a `(source, sender)` pair.
/// Events sent on the [`mpsc::UnboundedSender`] side are consumed by
/// [`L1HeadSource::next`]. When all senders are dropped, `next` returns
/// [`SourceError::Closed`].
#[derive(Debug)]
pub struct ChannelL1HeadSource {
    rx: mpsc::UnboundedReceiver<L1HeadEvent>,
}

impl ChannelL1HeadSource {
    /// Create a new channel L1 head source and its corresponding sender handle.
    pub fn new() -> (Self, mpsc::UnboundedSender<L1HeadEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl L1HeadSource for ChannelL1HeadSource {
    async fn next(&mut self) -> Result<L1HeadEvent, SourceError> {
        self.rx.recv().await.ok_or(SourceError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receive_l1_head_event() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        tx.send(L1HeadEvent::NewHead(42)).unwrap();

        let event = source.next().await.unwrap();
        assert_eq!(event, L1HeadEvent::NewHead(42));
    }

    #[tokio::test]
    async fn closed_when_sender_dropped() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        drop(tx);

        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Closed));
    }

    #[tokio::test]
    async fn parks_when_empty_then_receives() {
        let (mut source, tx) = ChannelL1HeadSource::new();

        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            tx.send(L1HeadEvent::NewHead(99)).unwrap();
        });

        let event = source.next().await.unwrap();
        assert_eq!(event, L1HeadEvent::NewHead(99));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn fifo_ordering() {
        let (mut source, tx) = ChannelL1HeadSource::new();
        tx.send(L1HeadEvent::NewHead(1)).unwrap();
        tx.send(L1HeadEvent::NewHead(2)).unwrap();
        tx.send(L1HeadEvent::NewHead(3)).unwrap();

        assert_eq!(source.next().await.unwrap(), L1HeadEvent::NewHead(1));
        assert_eq!(source.next().await.unwrap(), L1HeadEvent::NewHead(2));
        assert_eq!(source.next().await.unwrap(), L1HeadEvent::NewHead(3));
    }
}
