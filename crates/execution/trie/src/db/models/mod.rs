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
mod key;
pub use key::*;
mod value;
pub use value::*;
mod change_set;
mod kv;
use std::fmt;

use alloy_primitives::{B256, BlockNumber};
pub use change_set::*;
pub use kv::*;
use reth_db::{
    BlockNumberList, TableSet, TableType, TableViewer,
    table::{DupSort, TableInfo},
    tables,
};
use reth_primitives_traits::{Account, StorageEntry};
use reth_trie_common::{BranchNodeCompact, StorageTrieEntry, StoredNibbles, StoredNibblesSubKey};

tables! {
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

    /// Tracks the active proof window for the V2 schema.
    table V2ProofWindow {
      type Key = ProofWindowKey;
      type Value = BlockNumberHash;
    }

    /// V2 hashed account history bitmap shards.
    table V2HashedAccountsHistory {
        type Key = HashedAccountShardedKey;
        type Value = BlockNumberList;
    }

    /// V2 hashed account old-value changesets.
    table V2HashedAccountChangeSets {
        type Key = BlockNumber;
        type Value = HashedAccountBeforeTx;
        type SubKey = B256;
    }

    /// V2 hashed account current state.
    table V2HashedAccounts {
        type Key = B256;
        type Value = Account;
    }

    /// V2 hashed storage history bitmap shards.
    table V2HashedStoragesHistory {
        type Key = HashedStorageShardedKey;
        type Value = BlockNumberList;
    }

    /// V2 hashed storage old-value changesets.
    table V2HashedStorageChangeSets {
        type Key = BlockNumberHashedAddress;
        type Value = StorageEntry;
        type SubKey = B256;
    }

    /// V2 hashed storage current state.
    table V2HashedStorages {
        type Key = B256;
        type Value = StorageEntry;
        type SubKey = B256;
    }

    /// V2 account trie history bitmap shards.
    table V2AccountsTrieHistory {
        type Key = AccountTrieShardedKey;
        type Value = BlockNumberList;
    }

    /// V2 account trie old-value changesets.
    table V2AccountTrieChangeSets {
        type Key = BlockNumber;
        type Value = TrieChangeSetsEntry;
        type SubKey = StoredNibblesSubKey;
    }

    /// V2 account trie current state.
    table V2AccountsTrie {
        type Key = StoredNibbles;
        type Value = BranchNodeCompact;
    }

    /// V2 storage trie history bitmap shards.
    table V2StoragesTrieHistory {
        type Key = StorageTrieShardedKey;
        type Value = BlockNumberList;
    }

    /// V2 storage trie old-value changesets.
    table V2StorageTrieChangeSets {
        type Key = BlockNumberHashedAddress;
        type Value = TrieChangeSetsEntry;
        type SubKey = StoredNibblesSubKey;
    }

    /// V2 storage trie current state.
    table V2StoragesTrie {
        type Key = B256;
        type Value = StorageTrieEntry;
        type SubKey = StoredNibblesSubKey;
    }
}
