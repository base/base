//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_common_consensus::BaseBlock;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug, Clone)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<BaseBlock>),
    /// An L2 reorg was detected.
    Reorg,
}
