//! MDBX implementation of [`BaseProofsStore`](crate::BaseProofsStore).
//!
//! This module provides a complete MDBX implementation of the
//! [`BaseProofsStore`](crate::BaseProofsStore) trait. It uses the [`reth_db`] crate for
//! database interactions and defines the necessary tables and models for storing trie branches,
//! accounts, and storage leaves.

mod block;
pub use block::*;
mod version;
pub use version::*;
mod storage;
pub use storage::*;
mod change_set;
mod kv;
use std::fmt;

use alloy_primitives::B256;
pub use change_set::*;
pub use kv::*;
use reth_db::{
    TableSet, TableType, TableViewer,
    table::{DupSort, Table, TableInfo, Value},
};
use reth_primitives_traits::Account;
use reth_trie_common::{BranchNodeCompact, StoredNibbles};

macro_rules! proof_tables {
    (@bool) => { false };
    (@bool $($t:tt)+) => { true };
    (@view $name:ident $v:ident) => { $v.view::<$name>() };
    (@view $name:ident $v:ident $_subkey:ty) => { $v.view_dupsort::<$name>() };
    ($($(#[$attr:meta])* table $name:ident { type Key = $key:ty; type Value = $value:ty; $(type SubKey = $subkey:ty;)? } )*) => {
        $(
            $(#[$attr])*
            pub struct $name {
                _private: std::marker::PhantomData<()>,
            }
            impl fmt::Debug for $name {
                fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                    unreachable!("this type cannot be instantiated")
                }
            }
            impl Table for $name where $value: Value + 'static {
                const NAME: &'static str = stringify!($name);
                const DUPSORT: bool = proof_tables!(@bool $($subkey)?);
                type Key = $key;
                type Value = $value;
            }
            $(impl DupSort for $name { type SubKey = $subkey; })?
        )*
        /// A table in the proof database.
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Tables { $(#[doc = concat!("The [`", stringify!($name), "`] database table.")] $name,)* }
        impl Tables {
            /// All the tables in the database.
            pub const ALL: &'static [Self] = &[$(Self::$name,)*];
            /// The number of tables in the database.
            pub const COUNT: usize = Self::ALL.len();
            /// Returns the name of the table as a string.
            pub const fn name(&self) -> &'static str { match self { $(Self::$name => stringify!($name),)* } }
            /// Returns `true` if the table is a `DUPSORT` table.
            pub const fn is_dupsort(&self) -> bool { match self { $(Self::$name => proof_tables!(@bool $($subkey)?),)* } }
            /// The type of the given table in database.
            pub const fn table_type(&self) -> TableType { if self.is_dupsort() { TableType::DupSort } else { TableType::Table } }
            /// Allows to operate on specific table type.
            pub fn view<T, R>(&self, visitor: &T) -> Result<R, T::Error> where T: ?Sized + TableViewer<R> { match self { $(Self::$name => proof_tables!(@view $name visitor $($subkey)?),)* } }
        }
        impl fmt::Debug for Tables { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.name()) } }
        impl fmt::Display for Tables { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.name().fmt(f) } }
        impl std::str::FromStr for Tables {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> { match s { $(stringify!($name) => Ok(Self::$name),)* s => Err(format!("unknown table: {s:?}")) } }
        }
        impl TableInfo for Tables { fn name(&self) -> &'static str { self.name() } fn is_dupsort(&self) -> bool { self.is_dupsort() } }
        impl TableSet for Tables { fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> { Box::new(Self::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>)) } }
    };
}

proof_tables! {
    /// Stores historical branch nodes for the account state trie.
    ///
    /// Each entry maps a compact-encoded trie path (`StoredNibbles`) to its versioned branch node.
    /// Multiple versions of the same node are stored using the block number as a subkey.
    table AccountTrieHistory {
        type Key = StoredNibbles;
        type Value = VersionedValue<BranchNodeCompact>;
        type SubKey = u64; // block number
    }

    /// Stores historical branch nodes for the storage trie of each account.
    ///
    /// Each entry is identified by a composite key combining the account’s hashed address and the
    /// compact-encoded trie path. Versions are tracked using block numbers as subkeys.
    table StorageTrieHistory {
        type Key = StorageTrieKey;
        type Value = VersionedValue<BranchNodeCompact>;
        type SubKey = u64; // block number
    }

    /// Stores versioned account state across block history.
    ///
    /// Each entry maps a hashed account address to its serialized account data (balance, nonce,
    /// code hash, storage root).
    table HashedAccountHistory {
        type Key = B256;
        type Value = VersionedValue<Account>;
        type SubKey = u64; // block number
    }

    /// Stores versioned storage state across block history.
    ///
    /// Each entry maps a composite key of (hashed address, storage key) to its stored value.
    /// Used for reconstructing contract storage at any historical block height.
    table HashedStorageHistory {
        type Key = HashedStorageKey;
        type Value = VersionedValue<StorageValue>;
        type SubKey = u64; // block number
    }

    /// Tracks the active proof window in the external historical storage.
    ///
    /// Stores the earliest and latest block numbers (and corresponding hashes)
    /// for which historical trie data is retained.
    table ProofWindow {
      type Key = ProofWindowKey;
      type Value = BlockNumberHash;
    }

    /// A reverse mapping of block numbers to a keys of the tables.
    /// This is used for efficiently locating data by block number.
    table BlockChangeSet {
        type Key = u64; // Block number
        type Value = ChangeSet;
    }
}
