use std::collections::VecDeque;

use base_common_consensus::BaseBlock;

/// A pre-built queue of [`BaseBlock`]s for the batcher to drain.
///
/// Tests push fully-formed blocks into the source, which the batcher consumes one at a
/// time via [`next_block`](Self::next_block). The batcher extracts the L1 epoch from the
/// first (deposit) transaction in each block, filters out all deposit transactions, and
/// encodes the remaining user transactions into a `SingleBatch` for submission.
#[derive(Debug, Default)]
pub struct ActionL2Source {
    blocks: VecDeque<BaseBlock>,
}

impl ActionL2Source {
    /// Create an empty source.
    pub const fn new() -> Self {
        Self { blocks: VecDeque::new() }
    }

    /// Create a source containing the supplied blocks in iteration order.
    pub fn from_blocks(blocks: impl IntoIterator<Item = BaseBlock>) -> Self {
        let mut source = Self::new();
        source.extend(blocks);
        source
    }

    /// Push a block to the back of the queue.
    pub fn push(&mut self, block: BaseBlock) {
        self.blocks.push_back(block);
    }

    /// Return the next L2 block, or `None` if the source is exhausted.
    pub fn next_block(&mut self) -> Option<BaseBlock> {
        self.blocks.pop_front()
    }

    /// Return the number of blocks remaining.
    pub fn remaining(&self) -> usize {
        self.blocks.len()
    }

    /// Return `true` if the source has been fully drained.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl Extend<BaseBlock> for ActionL2Source {
    fn extend<T: IntoIterator<Item = BaseBlock>>(&mut self, iter: T) {
        self.blocks.extend(iter);
    }
}

impl FromIterator<BaseBlock> for ActionL2Source {
    fn from_iter<T: IntoIterator<Item = BaseBlock>>(iter: T) -> Self {
        Self::from_blocks(iter)
    }
}
