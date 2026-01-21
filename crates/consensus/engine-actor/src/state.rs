//! Engine sync state tracking.

use std::sync::Arc;

use alloy_primitives::B256;
use kona_protocol::L2BlockInfo;
use parking_lot::RwLock;

/// Tracks the engine synchronization state.
///
/// This maintains the current unsafe, safe, and finalized heads,
/// which are updated as the engine processes blocks.
#[derive(Debug, Clone)]
pub struct EngineSyncState {
    inner: Arc<RwLock<EngineSyncStateInner>>,
}

/// Inner state for the engine sync state.
#[derive(Debug, Default)]
struct EngineSyncStateInner {
    /// The current unsafe head.
    unsafe_head: Option<L2BlockInfo>,
    /// The current safe head.
    safe_head: Option<L2BlockInfo>,
    /// The current finalized head.
    finalized_head: Option<L2BlockInfo>,
}

impl Default for EngineSyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineSyncState {
    /// Creates a new engine sync state.
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(EngineSyncStateInner::default())) }
    }

    /// Sets the unsafe head.
    pub fn set_unsafe_head(&self, head: L2BlockInfo) {
        self.inner.write().unsafe_head = Some(head);
    }

    /// Sets the safe head.
    pub fn set_safe_head(&self, head: L2BlockInfo) {
        self.inner.write().safe_head = Some(head);
    }

    /// Sets the finalized head.
    pub fn set_finalized_head(&self, head: L2BlockInfo) {
        self.inner.write().finalized_head = Some(head);
    }

    /// Returns the current unsafe head.
    pub fn unsafe_head(&self) -> Option<L2BlockInfo> {
        self.inner.read().unsafe_head
    }

    /// Returns the current safe head.
    pub fn safe_head(&self) -> Option<L2BlockInfo> {
        self.inner.read().safe_head
    }

    /// Returns the current finalized head.
    pub fn finalized_head(&self) -> Option<L2BlockInfo> {
        self.inner.read().finalized_head
    }

    /// Returns the unsafe head block hash.
    pub fn unsafe_head_hash(&self) -> Option<B256> {
        self.inner.read().unsafe_head.map(|h| h.block_info.hash)
    }

    /// Returns the safe head block hash.
    pub fn safe_head_hash(&self) -> Option<B256> {
        self.inner.read().safe_head.map(|h| h.block_info.hash)
    }

    /// Returns the finalized head block hash.
    pub fn finalized_head_hash(&self) -> Option<B256> {
        self.inner.read().finalized_head.map(|h| h.block_info.hash)
    }

    /// Returns `true` if the sync state has been initialized.
    pub fn is_initialized(&self) -> bool {
        let inner = self.inner.read();
        inner.unsafe_head.is_some() && inner.safe_head.is_some() && inner.finalized_head.is_some()
    }
}
