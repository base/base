//! Forkchoice state tracking.

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use parking_lot::RwLock;

/// Tracks the current forkchoice state (unsafe, safe, finalized heads).
///
/// This struct maintains the latest forkchoice state received from FCU responses,
/// allowing query methods to return the current L2 chain state.
#[derive(Debug, Clone)]
pub struct ForkchoiceTracker {
    inner: Arc<RwLock<ForkchoiceStateInner>>,
}

/// Inner state for the forkchoice tracker.
#[derive(Debug, Default)]
struct ForkchoiceStateInner {
    /// The current forkchoice state.
    state: Option<ForkchoiceState>,
}

impl Default for ForkchoiceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ForkchoiceTracker {
    /// Creates a new forkchoice tracker.
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(ForkchoiceStateInner::default())) }
    }

    /// Updates the forkchoice state.
    pub fn update(&self, state: ForkchoiceState) {
        let mut inner = self.inner.write();
        inner.state = Some(state);
    }

    /// Returns the current forkchoice state, if set.
    pub fn state(&self) -> Option<ForkchoiceState> {
        self.inner.read().state
    }

    /// Returns the unsafe head block hash.
    pub fn unsafe_head(&self) -> Option<B256> {
        self.inner.read().state.map(|s| s.head_block_hash)
    }

    /// Returns the safe head block hash.
    pub fn safe_head(&self) -> Option<B256> {
        self.inner.read().state.map(|s| s.safe_block_hash)
    }

    /// Returns the finalized head block hash.
    pub fn finalized_head(&self) -> Option<B256> {
        self.inner.read().state.map(|s| s.finalized_block_hash)
    }

    /// Returns `true` if the forkchoice state has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.inner.read().state.is_some()
    }
}
