//! In-memory `SafeDB` fakes for deterministic startup and progression assertions.
//!
//! The fake stores L1→safe-L2 mappings in shared in-memory state and supports pre-population to
//! model restart scenarios.

use std::sync::Arc;

use async_trait::async_trait;
use base_consensus_safedb::{SafeDBError, SafeDBReader, SafeHeadListener, SafeHeadResponse};
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Default)]
struct FakeSafeDBState {
    entries: Vec<SafeHeadResponse>,
}

/// Shared handle for querying and pre-populating fake `SafeDB` state.
#[derive(Clone, Debug)]
pub struct FakeSafeDBHandle {
    state: Arc<Mutex<FakeSafeDBState>>,
}

impl FakeSafeDBHandle {
    /// Returns all stored responses.
    pub async fn entries(&self) -> Vec<SafeHeadResponse> {
        self.state.lock().await.entries.clone()
    }

    /// Returns the latest stored safe head, if any.
    pub async fn latest(&self) -> Option<SafeHeadResponse> {
        self.state.lock().await.entries.last().copied()
    }

    /// Inserts pre-populated responses.
    pub async fn prepopulate(&self, entries: impl IntoIterator<Item = SafeHeadResponse>) {
        self.state.lock().await.entries.extend(entries);
    }

    /// Blocking pre-population helper.
    pub fn prepopulate_blocking(&self, entries: impl IntoIterator<Item = SafeHeadResponse>) {
        self.state.blocking_lock().entries.extend(entries);
    }

    /// Blocking latest lookup helper.
    pub fn latest_blocking(&self) -> Option<SafeHeadResponse> {
        self.state.blocking_lock().entries.last().copied()
    }
}

/// In-memory fake implementing both `SafeHeadListener` and `SafeDBReader`.
#[derive(Clone, Debug)]
pub struct FakeSafeDB {
    state: Arc<Mutex<FakeSafeDBState>>,
}

impl Default for FakeSafeDB {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSafeDB {
    /// Creates an empty fake `SafeDB`.
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(FakeSafeDBState::default())) }
    }

    /// Creates a fake `SafeDB` with pre-populated entries.
    pub async fn with_entries(entries: impl IntoIterator<Item = SafeHeadResponse>) -> Self {
        let db = Self::new();
        db.handle().prepopulate(entries).await;
        db
    }

    /// Creates a fake `SafeDB` with pre-populated entries in blocking setup code.
    pub fn with_entries_blocking(entries: impl IntoIterator<Item = SafeHeadResponse>) -> Self {
        let db = Self::new();
        db.handle().prepopulate_blocking(entries);
        db
    }

    /// Returns a state handle.
    pub fn handle(&self) -> FakeSafeDBHandle {
        FakeSafeDBHandle { state: Arc::clone(&self.state) }
    }
}

#[async_trait]
impl SafeHeadListener for FakeSafeDB {
    async fn safe_head_updated(
        &self,
        safe_head: L2BlockInfo,
        l1_block: BlockInfo,
    ) -> Result<(), SafeDBError> {
        let mut state = self.state.lock().await;
        state.entries.push(SafeHeadResponse { l1_block: l1_block.id(), safe_head: safe_head.block_info.id() });
        Ok(())
    }

    async fn safe_head_reset(&self, reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError> {
        let mut state = self.state.lock().await;
        let reset_l1 = reset_safe_head.l1_origin.number;
        state.entries.retain(|entry| entry.l1_block.number < reset_l1);
        state.entries.push(SafeHeadResponse {
            l1_block: reset_safe_head.l1_origin,
            safe_head: reset_safe_head.block_info.id(),
        });
        Ok(())
    }
}

#[async_trait]
impl SafeDBReader for FakeSafeDB {
    async fn safe_head_at_l1(&self, l1_block_num: u64) -> Result<SafeHeadResponse, SafeDBError> {
        let state = self.state.lock().await;
        state
            .entries
            .iter()
            .rev()
            .find(|entry| entry.l1_block.number <= l1_block_num)
            .copied()
            .ok_or(SafeDBError::NotFound)
    }
}
