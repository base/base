//! Storage metadata models.

use base_common_types_chain::{Compact, add_arbitrary_tests};
use serde::{Deserialize, Serialize};

/// Persisted storage layout marker. Only the v2 layout is supported.
///
/// The boolean is retained to detect incompatible databases created with storage v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Compact, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StorageSettings {
    /// Whether the persisted database uses the supported v2 storage layout.
    pub storage_v2: bool,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self::v2()
    }
}

impl StorageSettings {
    /// Returns the storage layout used by Base.
    pub const fn base() -> Self {
        Self::v2()
    }

    /// Creates the supported storage layout settings.
    pub const fn v2() -> Self {
        Self { storage_v2: true }
    }
}

/// Marker for an in-progress partial state trie unwind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialStateTrieUnwindMarker {
    /// The Finish stage block number before the unwind started.
    pub finish_block_number: u64,
    /// The partial state trie frontier the pipeline is unwinding to.
    pub partial_state_trie: u64,
}
