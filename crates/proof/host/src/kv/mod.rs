//! This module contains the [`KeyValueStore`] trait and concrete implementations of it.

use std::sync::Arc;

use alloy_primitives::B256;
use tokio::sync::RwLock;

use crate::Result;

mod mem;
pub use mem::MemoryKeyValueStore;

mod split;
pub use split::SplitKeyValueStore;

#[cfg(feature = "disk")]
mod disk;
#[cfg(feature = "disk")]
pub use disk::DiskKeyValueStore;

/// A type alias for a shared key-value store.
pub type SharedKeyValueStore = Arc<RwLock<dyn KeyValueStore + Send + Sync>>;

/// Describes the interface of a simple, synchronous key-value store.
pub trait KeyValueStore {
    /// Get the value associated with the given key.
    fn get(&self, key: B256) -> Option<Vec<u8>>;

    /// Set the value associated with the given key.
    fn set(&mut self, key: B256, value: Vec<u8>) -> Result<()>;
}
