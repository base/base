//! Pending-frame observer hook for flashblocks.

use crate::PendingBlocks;

/// Synchronous observer for newly advanced pending flashblock frames.
pub trait PendingFrameObserver: Send + Sync + std::fmt::Debug {
    /// Observe a newly built pending-frame snapshot.
    fn on_pending_frame(&self, pending: &PendingBlocks);
}
