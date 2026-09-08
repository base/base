#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// The implementation of forward-only in-memory cursor.
pub mod forward_cursor;

/// The cursor implementations for navigating account and storage tries.
pub mod trie_cursor;

/// The cursor implementations for navigating hashed state.
pub mod hashed_cursor;

/// The trie walker for iterating over the trie nodes.
pub mod walker;

/// The iterators for traversing existing intermediate hashes and updated trie leaves.
pub mod node_iter;

/// Merkle proof generation.
pub mod proof;

/// Merkle proof generation v2 (leaf-only implementation).
pub mod proof_v2;

/// Trie witness generation.
pub mod witness;

/// Trie changeset computation.
pub mod changesets;

/// The implementation of the Merkle Patricia Trie.
mod trie;
pub use trie::{StateRoot, StorageRoot, TrieType};

/// Utilities for state root checkpoint progress.
mod progress;
pub use progress::{
    IntermediateRootState, IntermediateStateRootState, IntermediateStorageRootState,
    StateRootProgress, StorageRootProgress,
};

/// Trie calculation stats.
pub mod stats;

// re-export for convenience
pub use reth_trie_common::*;

/// Trie calculation metrics.
#[cfg(feature = "metrics")]
pub mod metrics;

/// Collection of trie-related test utilities.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// Collection of mock types for testing.
#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

/// Verification of existing stored trie nodes against state data.
pub mod verify;

mod database_changesets;
pub use database_changesets::*;

mod database_hashed_cursor;
pub use database_hashed_cursor::{
    DatabaseHashedAccountCursor, DatabaseHashedCursorFactory, DatabaseHashedStorageCursor,
};
mod database_prefix_set;
pub use database_prefix_set::load_prefix_sets_with_provider;
mod database_proof;
pub use database_proof::{DatabaseProof, DatabaseStorageProof};
mod database_state;
pub use database_state::{DatabaseHashedPostState, DatabaseStateRoot};
mod database_storage;
pub use database_storage::{DatabaseStorageRoot, hashed_storage_from_reverts_with_provider};
mod database_trie_cursor;
pub use database_trie_cursor::{
    DatabaseAccountTrieCursor, DatabaseStorageTrieCursor, DatabaseTrieCursorFactory,
    PackedKeyAdapter, StorageTrieEntryLike, TrieKeyAdapter, TrieTableAdapter,
};
pub use reth_db_api::tables::{PackedAccountsTrie, PackedStoragesTrie};
