//! Channel-backed [`UnsafeBlockSource`] for tests and in-process pipelines.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{L2BlockEvent, SourceError, UnsafeBlockSource};

/// An [`UnsafeBlockSource`] backed by a `tokio::sync::mpsc` unbounded channel.
///
/// Use [`ChannelBlockSource::new`] to obtain a `(source, sender)` pair.
/// Events sent on the [`mpsc::UnboundedSender`] side are consumed by
/// [`UnsafeBlockSource::next`].
#[derive(Debug)]
pub struct ChannelBlockSource {
    /// The receiving half of the unbounded channel.
    rx: mpsc::UnboundedReceiver<L2BlockEvent>,
}

impl ChannelBlockSource {
    /// Create a new channel block source and its corresponding sender handle.
    pub fn new() -> (Self, mpsc::UnboundedSender<L2BlockEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

#[async_trait]
impl UnsafeBlockSource for ChannelBlockSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        match self.rx.try_recv() {
            Ok(event) => return Ok(event),
            Err(mpsc::error::TryRecvError::Disconnected) => return Err(SourceError::Exhausted),
            Err(mpsc::error::TryRecvError::Empty) => {}
        }

        self.rx.recv().await.ok_or(SourceError::Exhausted)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_common_consensus::BaseBlock;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::*;

    /// Helper to build a minimal [`BaseBlock`] with a given number.
    fn make_block(number: u64) -> BaseBlock {
        BaseBlock {
            header: alloy_consensus::Header {
                number,
                parent_hash: B256::ZERO,
                ..Default::default()
            },
            body: Default::default(),
        }
    }

    /// Helper to build a minimal [`L2BlockInfo`].
    fn make_l2_block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo::new(BlockInfo::new(B256::ZERO, number, B256::ZERO, 0), Default::default(), 0)
    }

    #[tokio::test]
    async fn receive_block_event() {
        let (mut source, tx) = ChannelBlockSource::new();
        tx.send(L2BlockEvent::Block(Box::new(make_block(1)))).unwrap();

        let event = source.next().await.unwrap();
        match event {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 1),
            _ => panic!("expected Block event"),
        }
    }

    #[tokio::test]
    async fn receive_reorg_event() {
        let (mut source, tx) = ChannelBlockSource::new();
        let info = make_l2_block_info(5);
        tx.send(L2BlockEvent::Reorg { new_safe_head: info }).unwrap();

        let event = source.next().await.unwrap();
        match event {
            L2BlockEvent::Reorg { new_safe_head } => {
                assert_eq!(new_safe_head.block_info.number, 5);
            }
            _ => panic!("expected Reorg event"),
        }
    }

    #[tokio::test]
    async fn exhausted_when_sender_dropped() {
        let (mut source, tx) = ChannelBlockSource::new();
        drop(tx);

        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Exhausted));
    }

    #[tokio::test]
    async fn parks_when_empty_then_receives() {
        let (mut source, tx) = ChannelBlockSource::new();

        // Spawn a task that sends a block after yielding, so the receiver
        // must park on `recv().await` (the channel is empty at call time).
        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            tx.send(L2BlockEvent::Block(Box::new(make_block(42)))).unwrap();
        });

        let event = source.next().await.unwrap();
        match event {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 42),
            _ => panic!("expected Block event"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn fifo_ordering() {
        let (mut source, tx) = ChannelBlockSource::new();
        tx.send(L2BlockEvent::Block(Box::new(make_block(1)))).unwrap();
        tx.send(L2BlockEvent::Block(Box::new(make_block(2)))).unwrap();
        tx.send(L2BlockEvent::Reorg { new_safe_head: make_l2_block_info(0) }).unwrap();

        match source.next().await.unwrap() {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 1),
            _ => panic!("expected Block(1)"),
        }
        match source.next().await.unwrap() {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 2),
            _ => panic!("expected Block(2)"),
        }
        match source.next().await.unwrap() {
            L2BlockEvent::Reorg { new_safe_head } => {
                assert_eq!(new_safe_head.block_info.number, 0);
            }
            _ => panic!("expected Reorg"),
        }
    }
}
