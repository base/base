use alloy_primitives::{BlockHash, Bytes};
use std::collections::VecDeque;

use crate::L2BlockProvider;

/// A mock L2 block for use with the batcher actor.
///
/// Only the fields the batcher reads are present: block number, parent hash,
/// timestamp, the L1 block the sequencer was building on top of (the
/// "epoch"), and the raw encoded transactions included in the block.
///
/// All fields are deliberately public so tests can construct blocks inline
/// without going through a builder.
#[derive(Debug, Clone, Default)]
pub struct MockL2Block {
    /// L2 block number.
    pub number: u64,
    /// Hash of the previous L2 block. `B256::ZERO` for the first block.
    pub parent_hash: BlockHash,
    /// L2 block timestamp.
    pub timestamp: u64,
    /// L1 block number this L2 block was derived from (epoch number).
    pub l1_origin_number: u64,
    /// Hash of the L1 block this L2 block was derived from (epoch hash).
    pub l1_origin_hash: BlockHash,
    /// Raw EIP-2718 encoded transactions included in this block.
    pub transactions: Vec<Bytes>,
}

/// A pre-loaded queue of [`MockL2Block`]s for the batcher to drain.
///
/// Tests push blocks in advance and the batcher pops them one by one via
/// the [`L2BlockProvider`] trait. Once the queue is empty, the batcher
/// stops producing batches for that cycle.
#[derive(Debug, Default)]
pub struct MockL2Source {
    blocks: VecDeque<MockL2Block>,
}

impl MockL2Source {
    /// Create an empty [`MockL2Source`].
    pub const fn new() -> Self {
        Self { blocks: VecDeque::new() }
    }

    /// Create a [`MockL2Source`] from a pre-built list of blocks.
    pub fn from_blocks(blocks: Vec<MockL2Block>) -> Self {
        Self { blocks: VecDeque::from(blocks) }
    }

    /// Push a single block to the back of the queue.
    pub fn push(&mut self, block: MockL2Block) {
        self.blocks.push_back(block);
    }

    /// Generate and enqueue `count` sequential mock blocks.
    ///
    /// Blocks are numbered starting from `start_number` with timestamps
    /// advancing by `block_time` seconds each. Parent hashes are left as
    /// `B256::ZERO` — action tests that inspect parent-hash chaining should
    /// construct blocks manually.
    pub fn generate(
        &mut self,
        start_number: u64,
        start_timestamp: u64,
        block_time: u64,
        count: u64,
    ) {
        for i in 0..count {
            self.blocks.push_back(MockL2Block {
                number: start_number + i,
                timestamp: start_timestamp + i * block_time,
                ..Default::default()
            });
        }
    }

    /// Return the number of blocks remaining in the queue.
    pub fn remaining(&self) -> usize {
        self.blocks.len()
    }

    /// Return `true` if the source has been fully drained.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Peek at the next block without consuming it.
    pub fn peek(&self) -> Option<&MockL2Block> {
        self.blocks.front()
    }
}

impl L2BlockProvider for MockL2Source {
    fn next_block(&mut self) -> Option<MockL2Block> {
        self.blocks.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_returns_none() {
        let mut src = MockL2Source::new();
        assert!(src.next_block().is_none());
        assert!(src.peek().is_none());
    }

    #[test]
    fn blocks_returned_in_fifo_order() {
        let blocks = vec![
            MockL2Block { number: 0, ..Default::default() },
            MockL2Block { number: 1, ..Default::default() },
            MockL2Block { number: 2, ..Default::default() },
        ];
        let mut src = MockL2Source::from_blocks(blocks);
        assert_eq!(src.next_block().unwrap().number, 0);
        assert_eq!(src.next_block().unwrap().number, 1);
        assert_eq!(src.next_block().unwrap().number, 2);
        assert!(src.next_block().is_none());
    }

    #[test]
    fn peek_does_not_consume() {
        let mut src = MockL2Source::new();
        src.push(MockL2Block { number: 7, ..Default::default() });
        assert_eq!(src.peek().unwrap().number, 7);
        assert_eq!(src.peek().unwrap().number, 7);
        assert_eq!(src.next_block().unwrap().number, 7);
        assert!(src.peek().is_none());
    }

    #[test]
    fn generate_produces_correct_numbers_and_timestamps() {
        let mut src = MockL2Source::new();
        src.generate(10, 1000, 2, 3);
        let b0 = src.next_block().unwrap();
        let b1 = src.next_block().unwrap();
        let b2 = src.next_block().unwrap();
        assert_eq!((b0.number, b0.timestamp), (10, 1000));
        assert_eq!((b1.number, b1.timestamp), (11, 1002));
        assert_eq!((b2.number, b2.timestamp), (12, 1004));
        assert!(src.is_empty());
    }
}
