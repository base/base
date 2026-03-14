//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_alloy_consensus::OpBlock;
use base_protocol::L2BlockInfo;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug, Clone)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<OpBlock>),
    /// An L2 reorg was detected; all state should be rewound to `new_safe_head`.
    Reorg {
        /// The new safe head after the reorg.
        new_safe_head: L2BlockInfo,
    },
}
