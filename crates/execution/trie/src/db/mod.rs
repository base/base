//! MDBX implementation of [`OpProofsStore`](crate::OpProofsStore).
//!
//! This module provides a complete MDBX implementation of the
//! [`OpProofsStore`](crate::OpProofsStore) trait. It uses the [`reth_db`]
//! crate for database interactions and defines the necessary tables and models for storing trie
//! branches, accounts, and storage leaves.

mod models;
pub use models::{
    AccountTrieHistory, BlockChangeSet, BlockNumberHash, ChangeSet, HashedAccountHistory,
    HashedStorageHistory, HashedStorageKey, IntoKV, MaybeDeleted, ProofWindow, ProofWindowKey,
    StorageTrieHistory, StorageTrieKey, StorageValue, Tables, VersionedValue,
};

mod store;
pub use store::MdbxProofsStorage;

mod cursor;
pub use cursor::{
    BlockNumberVersionedCursor, MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
};
