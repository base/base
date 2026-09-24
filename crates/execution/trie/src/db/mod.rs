//! Database-backed implementations of [`BaseProofsStore`](crate::BaseProofsStore).
//!
//! This module defines the schema models and RocksDB storage backend used for storing trie
//! branches, accounts, and storage leaves.

mod models;
pub use models::*;

mod rocksdb;
pub use rocksdb::{
    ProofWindowValue, RocksDbHistoryTable, RocksDbLatestVersionResult, RocksdbAccountCursor,
    RocksdbBatchSession, RocksdbHistoryDeleteBatch, RocksdbPreparedHistoryDeletes,
    RocksdbPreparedPrune, RocksdbProofsStorage, RocksdbProofsStorageOptions, RocksdbPrunePlan,
    RocksdbReadSnapshot, RocksdbReplacementState, RocksdbStorageCursor, RocksdbTrieCursor,
    RocksdbVersionedCursor,
};
