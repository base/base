//! MDBX implementation of [`BaseProofsStore`](crate::BaseProofsStore).
//!
//! This module provides a complete MDBX implementation of the
//! [`BaseProofsStore`](crate::BaseProofsStore) trait. It uses the [`reth_db`]
//! crate for database interactions and defines the necessary tables and models for storing trie
//! branches, accounts, and storage leaves.

mod models;
pub use models::*;

mod store;
pub use store::MdbxProofsStorage;

mod cursor;
pub use cursor::{
    BlockNumberVersionedCursor, Dup, MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
    MdbxV2AccountCursor, MdbxV2AccountCursorEither, MdbxV2AccountTrieCursor,
    MdbxV2AccountTrieCursorEither, MdbxV2LatestAccountCursor, MdbxV2LatestAccountTrieCursor,
    MdbxV2LatestStorageCursor, MdbxV2LatestStorageTrieCursor, MdbxV2StorageCursor,
    MdbxV2StorageCursorEither, MdbxV2StorageTrieCursor, MdbxV2StorageTrieCursorEither,
};

mod batch;
pub use batch::MdbxBatchSession;
