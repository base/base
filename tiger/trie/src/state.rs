//! Hashed state changes and trie updates.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_trie::{BranchNodeCompact, EMPTY_ROOT_HASH, KECCAK_EMPTY, Nibbles};
use base_tiger_store::{Account, MdbxError};

/// A trie or state database failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrieError {
    /// The database refused.
    Mdbx(MdbxError),
    /// Stored data is inconsistent.
    Corrupt(String),
    /// A witness lacks a node the operation needs.
    MissingNode {
        /// The owning account's hashed address, `None` for the account trie.
        owner: Option<B256>,
        /// The node's path.
        path: Nibbles,
        /// The node's hash.
        hash: B256,
    },
    /// The state the operation needs is not held locally (partial state).
    Unknown(String),
    /// A block deletes an account whose storage a partial state does not hold, so the state
    /// cannot record what the wipe removes and can never commit that block.
    UnheldWipe {
        /// The account whose storage the block wipes.
        address: Address,
        /// The block that wipes it.
        block: u64,
    },
}

impl std::fmt::Display for TrieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mdbx(err) => write!(f, "{err}"),
            Self::Corrupt(text) => write!(f, "corrupt trie data: {text}"),
            Self::MissingNode { owner, path, hash } => match owner {
                Some(owner) => write!(f, "missing storage trie node {hash} at path {path:?} of account {owner}"),
                None => write!(f, "missing account trie node {hash} at path {path:?}"),
            },
            Self::Unknown(text) => write!(f, "state not held locally: {text}"),
            Self::UnheldWipe { address, block } => {
                write!(f, "state not held locally: storage of {address} wiped by block {block}")
            }
        }
    }
}

impl std::error::Error for TrieError {}

impl TrieError {
    /// Whether the operation needs state that is not held locally (a partial state or an
    /// incomplete witness), rather than failing on corrupt or unreadable data.
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::MissingNode { .. } | Self::Unknown(_) | Self::UnheldWipe { .. })
    }
}

impl From<MdbxError> for TrieError {
    fn from(err: MdbxError) -> Self {
        Self::Mdbx(err)
    }
}

impl From<base_tiger_store::CodecError> for TrieError {
    fn from(err: base_tiger_store::CodecError) -> Self {
        Self::Corrupt(err.to_string())
    }
}

/// One account's storage changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HashedStorage {
    /// Storage was cleared before `slots` apply.
    pub wiped: bool,
    /// Slot values by hashed slot; zero removes the slot.
    pub slots: BTreeMap<B256, U256>,
}

/// State changes in hashed-key space: what one or more blocks did, relative to a base state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HashedState {
    /// Accounts by hashed address; `None` deletes the account (and its storage).
    pub accounts: BTreeMap<B256, Option<Account>>,
    /// Storage changes by hashed address.
    pub storages: BTreeMap<B256, HashedStorage>,
}

impl HashedState {
    /// Sets an account.
    pub fn set_account(&mut self, key: B256, account: Account) {
        self.accounts.insert(key, Some(account));
    }

    /// Deletes an account with its storage.
    pub fn delete_account(&mut self, key: B256) {
        self.accounts.insert(key, None);
        self.storages.insert(key, HashedStorage { wiped: true, slots: BTreeMap::new() });
    }

    /// Clears an account's storage.
    pub fn wipe_storage(&mut self, key: B256) {
        self.storages.insert(key, HashedStorage { wiped: true, slots: BTreeMap::new() });
    }

    /// Sets a slot (zero removes it).
    pub fn set_slot(&mut self, key: B256, slot: B256, value: U256) {
        self.storages.entry(key).or_default().slots.insert(slot, value);
    }

    /// Applies `later` on top of `self`.
    pub fn extend(&mut self, later: &Self) {
        for (key, account) in &later.accounts {
            self.accounts.insert(*key, *account);
        }
        for (key, storage) in &later.storages {
            if storage.wiped {
                self.storages.insert(*key, storage.clone());
            } else {
                let entry = self.storages.entry(*key).or_default();
                entry.slots.extend(storage.slots.iter().map(|(k, v)| (*k, *v)));
            }
        }
    }

    /// Whether nothing changes.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.storages.is_empty()
    }
}

/// Changed trie nodes of one storage trie.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageTrieUpdates {
    /// Every stored node of the trie is removed before `nodes` apply.
    pub wiped: bool,
    /// Nodes by path; `None` removes the stored node.
    pub nodes: BTreeMap<Nibbles, Option<BranchNodeCompact>>,
}

/// Changed trie nodes, ready to write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrieUpdates {
    /// Account trie nodes by path; `None` removes the stored node.
    pub account_nodes: BTreeMap<Nibbles, Option<BranchNodeCompact>>,
    /// Storage trie changes by hashed address.
    pub storage_tries: BTreeMap<B256, StorageTrieUpdates>,
}

impl TrieUpdates {
    /// Applies `later` (node changes made on top of the trie these lead to) on top of `self`.
    pub fn extend(&mut self, later: &Self) {
        self.account_nodes.extend(later.account_nodes.iter().map(|(path, node)| (*path, node.clone())));
        for (address, storage) in &later.storage_tries {
            if storage.wiped {
                self.storage_tries.insert(*address, storage.clone());
            } else {
                let entry = self.storage_tries.entry(*address).or_default();
                entry.nodes.extend(storage.nodes.iter().map(|(path, node)| (*path, node.clone())));
            }
        }
    }

    /// Merges a hash builder's updated nodes and a walker's stale paths as reth's
    /// `finalize` + `into_sorted` do: updated nodes win over removals, and the root path is
    /// never stored.
    pub fn merge_nodes(
        target: &mut BTreeMap<Nibbles, Option<BranchNodeCompact>>,
        updated: impl IntoIterator<Item = (Nibbles, BranchNodeCompact)>,
        removed: impl IntoIterator<Item = Nibbles>,
    ) {
        let updated: BTreeMap<Nibbles, BranchNodeCompact> = updated.into_iter().filter(|(p, _)| !p.is_empty()).collect();
        for path in removed.into_iter().filter(|p| !p.is_empty()) {
            if !updated.contains_key(&path) {
                target.insert(path, None);
            }
        }
        for (path, node) in updated {
            target.insert(path, Some(node));
        }
    }
}

/// The account trie leaf value: `rlp([nonce, balance, storage_root, code_hash])`.
pub fn account_leaf(account: &Account, storage_root: B256) -> Vec<u8> {
    let code_hash = account.bytecode_hash.unwrap_or(KECCAK_EMPTY);
    let payload = account.nonce.length() + account.balance.length() + storage_root.length() + code_hash.length();
    let mut out = Vec::with_capacity(payload + 3);
    alloy_rlp::Header { list: true, payload_length: payload }.encode(&mut out);
    account.nonce.encode(&mut out);
    account.balance.encode(&mut out);
    storage_root.encode(&mut out);
    code_hash.encode(&mut out);
    out
}

/// The storage trie leaf value: the RLP of the value as an integer.
pub fn storage_leaf(value: U256) -> Vec<u8> {
    alloy_rlp::encode(value)
}

/// The keccak of an address or slot, as the trie keys it.
pub fn hash_key(bytes: &[u8]) -> B256 {
    keccak256(bytes)
}

/// The root of an empty trie.
pub const EMPTY_ROOT: B256 = EMPTY_ROOT_HASH;

/// Keys a set of changes touches, as nibble paths, for prefix sets.
pub fn nibble_keys<'a>(keys: impl IntoIterator<Item = &'a B256>) -> Vec<Nibbles> {
    let set: BTreeSet<&B256> = keys.into_iter().collect();
    set.into_iter().map(Nibbles::unpack).collect()
}
