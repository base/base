//! Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].

use base_common_consensus::BaseBlock;
use tokio::sync::oneshot;

/// Events emitted by an [`UnsafeBlockSource`][crate::UnsafeBlockSource].
///
/// No longer `Clone`: [`Flush`](Self::Flush)'s acknowledgement is a one-shot
/// [`oneshot::Sender`], which can't be cloned. Nothing in this workspace clones
/// `L2BlockEvent` today; if a future caller needs both, prefer redesigning around that need
/// (e.g. a shared completion handle) over re-adding `Clone` by wrapping the sender in
/// `Arc<Mutex<Option<_>>>`, which would let the ack silently fire multiple times or never.
#[derive(Debug)]
pub enum L2BlockEvent {
    /// A new unsafe L2 block arrived.
    Block(Box<BaseBlock>),
    /// An L2 reorg was detected.
    Reorg,
    /// Signal the driver to close the current channel and release pending
    /// frames as submissions without exhausting the source.
    ///
    /// The source remains open and the driver continues running, but the current
    /// channel is closed so all accumulated blocks become available for submission.
    Flush {
        /// Fired once the driver's encoding and submission are both fully drained (not just
        /// after the first frame). Delivered through this event — rather than a side channel
        /// — so the acknowledgement preserves the flush's ordering relative to the `Block`
        /// events that precede it in the same source.
        ///
        /// This is a "whole pipeline idle" signal, not scoped strictly to this flush's own
        /// frames: it never fires before them, but concurrent `Block` events arriving after
        /// this one can delay it further (see `base_batcher_core::AdminHandle::flush_and_wait`
        /// for the full caveat, which applies here identically).
        ack: Option<oneshot::Sender<()>>,
    },
}
