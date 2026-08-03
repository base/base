//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_common_consensus::BaseBlock;
use base_protocol::L2BlockInfo;
use tokio::sync::oneshot;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
#[derive(Debug)]
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
    /// Analogous to the reference batcher's `forcePublish` signal: the source remains
    /// open and the driver continues running, but the current channel is
    /// closed so all accumulated blocks become immediately available for
    /// submission.
    Flush {
        /// Fired once every frame resulting from this flush has been encoded and handed to
        /// the tx manager (not just the first). Delivered through this event — rather than a
        /// side channel — so the acknowledgement preserves the flush's ordering relative to
        /// the `Block` events that precede it in the same source.
        ack: Option<oneshot::Sender<()>>,
    },
}
