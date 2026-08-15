//! Database-backed implementations of [`BaseProofsStore`](crate::BaseProofsStore).
//!
//! This module defines the schema models and storage backends used for storing trie branches,
//! accounts, and storage leaves.

mod models;
pub use models::*;

mod store;
pub use store::{MdbxProofsStorage, MdbxProofsStorageOptions};

mod rocksdb;
pub use rocksdb::{
    ProofWindowValue, RocksDbHistoryTable, RocksDbLatestVersionResult, RocksdbAccountCursor,
    RocksdbBatchSession, RocksdbHistoryDeleteBatch, RocksdbPreparedHistoryDeletes,
    RocksdbPreparedPrune, RocksdbProofsStorage, RocksdbProofsStorageOptions, RocksdbPrunePlan,
    RocksdbReadSnapshot, RocksdbReplacementState, RocksdbStorageCursor, RocksdbTrieCursor,
    RocksdbVersionedCursor,
};

mod cursor;
pub use cursor::{
    BlockNumberVersionedCursor, Dup, MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
};

mod batch;
pub use batch::{DupRw, MdbxBatchSession};
