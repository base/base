//! Storage metadata models.

pub use base_execution_state_types::StorageSettings;
use serde::{Deserialize, Serialize};

/// Marker for an in-progress partial state trie unwind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialStateTrieUnwindMarker {
    /// The Finish stage block number before the unwind started.
    pub finish_block_number: u64,
    /// The partial state trie frontier the pipeline is unwinding to.
    pub partial_state_trie: u64,
}
