//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_common_consensus::BaseBlock;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug, Clone)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<BaseBlock>),
    /// An L2 reorg was detected.
    Reorg,
    /// Signal the driver to force-close the current channel and flush pending
    /// frames as submissions without exhausting the source.
    ///
    /// Analogous to the reference batcher's `forcePublish` signal: the source remains
    /// open and the driver continues running, but the current channel is
    /// closed so all accumulated blocks become immediately available for
    /// submission.
    Flush,
}
