//! Test utilities for block sources.

use std::collections::VecDeque;

use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_protocol::L2BlockInfo;

use crate::{L2BlockEvent, SourceError, UnsafeBlockSource};

/// In-memory block source for action tests and unit tests.
///
/// Events are pushed into a FIFO queue and drained by calls to
/// [`UnsafeBlockSource::next`].
#[derive(Debug)]
pub struct InMemoryBlockSource {
    /// FIFO queue of events to deliver.
    queue: VecDeque<L2BlockEvent>,
}

impl InMemoryBlockSource {
    /// Create a new empty in-memory block source.
    pub const fn new() -> Self {
        Self { queue: VecDeque::new() }
    }

    /// Push a block event into the queue.
    pub fn push_block(&mut self, block: OpBlock) {
        self.queue.push_back(L2BlockEvent::Block(Box::new(block)));
    }

    /// Push a reorg event into the queue.
    pub fn push_reorg(&mut self, new_safe_head: L2BlockInfo) {
        self.queue.push_back(L2BlockEvent::Reorg { new_safe_head });
    }
}

impl Default for InMemoryBlockSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UnsafeBlockSource for InMemoryBlockSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        self.queue.pop_front().ok_or(SourceError::Exhausted)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_protocol::BlockInfo;

    use super::*;

    /// Helper to build a minimal [`OpBlock`] with a given number and parent hash.
    fn make_block(number: u64, parent_hash: B256) -> OpBlock {
        OpBlock {
            header: alloy_consensus::Header { number, parent_hash, ..Default::default() },
            body: Default::default(),
        }
    }

    /// Helper to build a minimal [`L2BlockInfo`].
    fn make_l2_block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo::new(BlockInfo::new(B256::ZERO, number, B256::ZERO, 0), Default::default(), 0)
    }

    #[tokio::test]
    async fn test_in_memory_exhausted() {
        let mut source = InMemoryBlockSource::new();
        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Exhausted));
    }

    #[tokio::test]
    async fn test_in_memory_push_block() {
        let mut source = InMemoryBlockSource::new();
        let block = make_block(42, B256::ZERO);
        source.push_block(block);

        let event = source.next().await.unwrap();
        match event {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 42),
            _ => panic!("expected Block event"),
        }
    }

    #[tokio::test]
    async fn test_in_memory_push_reorg() {
        let mut source = InMemoryBlockSource::new();
        let info = make_l2_block_info(10);
        source.push_reorg(info);

        let event = source.next().await.unwrap();
        match event {
            L2BlockEvent::Reorg { new_safe_head } => {
                assert_eq!(new_safe_head.block_info.number, 10);
            }
            _ => panic!("expected Reorg event"),
        }
    }

    #[tokio::test]
    async fn test_in_memory_fifo() {
        let mut source = InMemoryBlockSource::new();
        source.push_block(make_block(1, B256::ZERO));
        source.push_block(make_block(2, B256::ZERO));
        source.push_reorg(make_l2_block_info(0));
        source.push_block(make_block(3, B256::ZERO));

        // First: Block(1)
        match source.next().await.unwrap() {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 1),
            _ => panic!("expected Block(1)"),
        }

        // Second: Block(2)
        match source.next().await.unwrap() {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 2),
            _ => panic!("expected Block(2)"),
        }

        // Third: Reorg
        match source.next().await.unwrap() {
            L2BlockEvent::Reorg { new_safe_head } => {
                assert_eq!(new_safe_head.block_info.number, 0);
            }
            _ => panic!("expected Reorg"),
        }

        // Fourth: Block(3)
        match source.next().await.unwrap() {
            L2BlockEvent::Block(b) => assert_eq!(b.header.number, 3),
            _ => panic!("expected Block(3)"),
        }

        // Exhausted
        assert!(matches!(source.next().await, Err(SourceError::Exhausted)));
    }
}
