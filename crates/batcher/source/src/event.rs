//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_common_consensus::BaseBlock;
use base_protocol::L2BlockInfo;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug, Clone)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<BaseBlock>),
    /// An L2 reorg was detected; all state should be rewound to `new_safe_head`.
    Reorg {
        /// The new safe head after the reorg.
        new_safe_head: L2BlockInfo,
    },
    /// Signal the driver to force-close the current channel and flush pending
    /// frames as submissions without exhausting the source.
    ///
    /// Analogous to op-batcher's `forcePublish` signal: the source remains
    /// open and the driver continues running, but the current channel is
    /// closed so all accumulated blocks become immediately available for
    /// submission.
    Flush,
}
