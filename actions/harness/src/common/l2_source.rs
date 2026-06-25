use std::collections::VecDeque;

use base_common_consensus::BaseBlock;

use crate::L2BlockProvider;

/// A pre-built queue of [`BaseBlock`]s for the batcher to drain.
///
/// Tests push fully-formed blocks into the source, which the batcher
/// consumes one at a time via [`L2BlockProvider::next_block`].
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

impl L2BlockProvider for ActionL2Source {
    fn next_block(&mut self) -> Option<BaseBlock> {
        self.blocks.pop_front()
    }
}
