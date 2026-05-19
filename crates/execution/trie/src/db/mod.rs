//! Database-backed implementations of [`BaseProofsStore`](crate::BaseProofsStore).
//!
//! This module defines the schema models and storage backends used for storing trie branches,
//! accounts, and storage leaves.

mod models;
pub use models::*;

mod store;
pub use store::MdbxProofsStorage;

mod rocksdb;
pub use rocksdb::{
    AccountTrieDomain, RocksDbDomain, RocksdbAccountCursor, RocksdbProofsCompression,
    RocksdbProofsStorage, RocksdbProofsStorageOptions, RocksdbReadSnapshot, RocksdbStorageCursor,
    RocksdbTrieCursor, StorageTrieDomain,
};

mod cursor;
pub use cursor::{
    BlockNumberVersionedCursor, Dup, MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
};
