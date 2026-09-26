use base_common_consensus::BaseBlock;

/// The [`BaseBlock`]s a batcher is created with, in order: its L2 chain so far.
///
/// Each block must start with its L1-info deposit: the batcher reads the block's L1 epoch
/// from it.
#[derive(Debug, Default)]
pub struct ActionL2Source {
    blocks: Vec<BaseBlock>,
}

impl ActionL2Source {
    /// Create an empty source.
    pub const fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Create a source containing the supplied blocks in iteration order.
    pub fn from_blocks(blocks: impl IntoIterator<Item = BaseBlock>) -> Self {
        let mut source = Self::new();
        source.extend(blocks);
        source
    }

    /// Append a block.
    pub fn push(&mut self, block: BaseBlock) {
        self.blocks.push(block);
    }
}

impl IntoIterator for ActionL2Source {
    type Item = BaseBlock;
    type IntoIter = std::vec::IntoIter<BaseBlock>;

    fn into_iter(self) -> Self::IntoIter {
        self.blocks.into_iter()
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
