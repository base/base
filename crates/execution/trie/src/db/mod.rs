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
};
