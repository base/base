//! Partial state: a trie of which only some paths are held locally, for a node that starts at a
//! recent block without state and fills it from each block's execution witness.
//!
//! The record of what is known is the stored node set itself. `TigerNodes` holds trie nodes as
//! RLP by path; a child referenced by a hash that is not stored is unknown. A flat-table miss is
//! resolved by walking the stored nodes: the walk ends at the leaf or proves the key absent
//! (known), or meets an unstored hash ([`TrieError::MissingNode`], unknown). Roots, updates and
//! proofs run [`SparseTrie`]s over the stored nodes, so a block needs only the paths it touches.
//! Such a datadir keeps reth's flat, changeset and history tables but not its `AccountsTrie` /
//! `StoragesTrie`, so only tiger opens it.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{EMPTY_ROOT_HASH, Nibbles, TrieAccount, nodes::TrieNode};
use base_tiger_store::{Account, BytecodeCodec, Db, Rw, StorageEntry, Table, TrieCodec, Txn};

use crate::{
    AccountProof, HashedState, NodeSource, SparseChanges, SparseTrie, StorageProof, TrieError, TrieView, account_leaf,
    storage_leaf,
};

/// The `Metadata` key whose presence marks a partial-state datadir; its value is the state root
/// of the stored tip.
pub const PARTIAL_ROOT_KEY: &[u8] = b"tiger_partial_state_root";

/// Most nodes one witness walk visits (a bound on malformed input).
const MAX_WITNESS_NODES: usize = 1 << 22;

fn decode_storage_leaf(leaf: &[u8]) -> Result<U256, TrieError> {
    U256::decode(&mut &leaf[..]).map_err(|e| TrieError::Corrupt(format!("storage leaf: {e}")))
}

/// Stored nodes of a partial-state datadir.
#[derive(Debug)]
pub struct DbNodes<'t, K> {
    /// The transaction.
    pub txn: &'t Txn<K>,
    /// The table handles.
    pub db: &'t Db,
}

impl<K> NodeSource for DbNodes<'_, K> {
    fn node(&self, owner: Option<B256>, path: &Nibbles, _hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
        Ok(self.txn.get(self.db.dbi(Table::TigerNodes), &PartialState::node_key(owner, path))?.map(<[u8]>::to_vec))
    }

    fn persisted(&self) -> bool {
        true
    }
}

/// Nodes of a full datadir (reth's tables, which keep no node RLP), each loaded as a proof of a
/// key through its path.
#[derive(Debug)]
pub struct ProofNodes<'v, 't, K> {
    /// The tables and the state over them the nodes belong to.
    pub view: &'v TrieView<'t, K>,
}

impl<K> NodeSource for ProofNodes<'_, '_, K> {
    fn node(&self, owner: Option<B256>, path: &Nibbles, hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
        let mut padded = [0u8; 64];
        for (i, nibble) in path.to_vec().iter().enumerate() {
            padded[i] = *nibble;
        }
        let key = B256::from_slice(&Nibbles::from_nibbles(padded).pack());
        let nodes = match owner {
            None => self.view.proof(key, &[])?.proof,
            Some(owner) => self.view.proof(owner, &[key])?.storage.pop().map(|s| s.proof).unwrap_or_default(),
        };
        Ok(nodes.into_iter().find(|node| keccak256(node) == hash).map(|node| node.to_vec()))
    }
}

/// A source that records every node it hands out (a witness of what an operation loaded).
#[derive(Debug)]
pub struct Recording<'s> {
    /// The source.
    pub inner: &'s dyn NodeSource,
    /// Nodes handed out, by hash.
    pub seen: RefCell<BTreeMap<B256, Bytes>>,
}

impl NodeSource for Recording<'_> {
    fn node(&self, owner: Option<B256>, path: &Nibbles, hash: B256) -> Result<Option<Vec<u8>>, TrieError> {
        let node = self.inner.node(owner, path, hash)?;
        if let Some(node) = &node {
            self.seen.borrow_mut().insert(hash, Bytes::from(node.clone()));
        }
        Ok(node)
    }

    fn persisted(&self) -> bool {
        self.inner.persisted()
    }
}

impl std::fmt::Debug for dyn NodeSource + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeSource")
    }
}

/// The account trie and the touched storage tries after applying hashed state changes.
#[derive(Debug)]
pub struct StateTries {
    /// The account trie.
    pub accounts: SparseTrie,
    /// Touched storage tries by hashed address, with whether the storage was wiped first.
    pub storages: BTreeMap<B256, (bool, SparseTrie)>,
    /// The state root after the changes.
    pub root: B256,
}

impl StateTries {
    /// Applies `state` to the trie with root `root`, loading nodes from `source`.
    pub fn apply(source: &dyn NodeSource, root: B256, state: &HashedState) -> Result<Self, TrieError> {
        let mut accounts = SparseTrie::new(None, root);
        let mut storages = BTreeMap::new();
        let keys: BTreeSet<B256> = state.accounts.keys().chain(state.storages.keys()).copied().collect();
        for key in keys {
            let current = accounts.get(source, key)?.map(|leaf| PartialState::decode_account_leaf(&leaf)).transpose()?;
            let account = match state.accounts.get(&key) {
                Some(account) => *account,
                None => current.map(|c| c.0),
            };
            let mut storage_root = current.map_or(EMPTY_ROOT_HASH, |c| c.1);
            if let Some(storage) = state.storages.get(&key) {
                let base = if storage.wiped { EMPTY_ROOT_HASH } else { storage_root };
                let mut trie = SparseTrie::new(Some(key), base);
                for (slot, value) in &storage.slots {
                    trie.update(source, *slot, (!value.is_zero()).then(|| storage_leaf(*value)))?;
                }
                storage_root = trie.root();
                // Storage of an account that ends absent is dropped, as reth drops it.
                if account.is_some() || storage.wiped {
                    storages.insert(key, (storage.wiped, trie));
                }
            }
            accounts.update(source, key, account.map(|a| account_leaf(&a, storage_root)))?;
        }
        let root = accounts.root();
        Ok(Self { accounts, storages, root })
    }

    /// The storage root of the account `key` after the changes (empty root when absent).
    pub fn storage_root(&mut self, source: &dyn NodeSource, key: B256) -> Result<B256, TrieError> {
        Ok(self.accounts.get(source, key)?.map(|leaf| PartialState::decode_account_leaf(&leaf)).transpose()?.map_or(EMPTY_ROOT_HASH, |c| c.1))
    }

    /// The proof of account `key` and its `slots` after the changes.
    pub fn proof(&mut self, source: &dyn NodeSource, key: B256, slots: &[B256]) -> Result<AccountProof, TrieError> {
        let proof = self.accounts.proof(source, key)?;
        let current = self.accounts.get(source, key)?.map(|leaf| PartialState::decode_account_leaf(&leaf)).transpose()?;
        let mut out = AccountProof {
            key,
            account: current.map(|c| c.0),
            storage_root: current.map_or(EMPTY_ROOT_HASH, |c| c.1),
            proof,
            storage: Vec::with_capacity(slots.len()),
        };
        let mut fresh;
        let trie = match self.storages.get_mut(&key) {
            Some((_, trie)) => trie,
            None => {
                fresh = SparseTrie::new(Some(key), out.storage_root);
                &mut fresh
            }
        };
        for slot in slots {
            if current.is_none() {
                out.storage.push(StorageProof { slot: *slot, value: U256::ZERO, proof: Vec::new() });
                continue;
            }
            let value = trie.get(source, *slot)?.map(|leaf| decode_storage_leaf(&leaf)).transpose()?.unwrap_or_default();
            out.storage.push(StorageProof { slot: *slot, value, proof: trie.proof(source, *slot)? });
        }
        Ok(out)
    }

    /// Writes the node changes to `TigerNodes` and the new root to `Metadata`.
    pub fn write(&self, txn: &mut Txn<Rw>, db: &Db) -> Result<(), TrieError> {
        let nodes = db.dbi(Table::TigerNodes);
        let put = |txn: &mut Txn<Rw>, owner: Option<B256>, changes: &SparseChanges| -> Result<(), TrieError> {
            for path in &changes.deletes {
                txn.delete(nodes, &PartialState::node_key(owner, path), None)?;
            }
            for (path, node) in &changes.writes {
                txn.put(nodes, &PartialState::node_key(owner, path), node)?;
            }
            Ok(())
        };
        for (key, (wiped, trie)) in &self.storages {
            if *wiped {
                Self::delete_owner(txn, db, *key)?;
            }
            put(txn, Some(*key), &trie.changes())?;
        }
        put(txn, None, &self.accounts.changes())?;
        if self.root == EMPTY_ROOT_HASH {
            txn.delete(nodes, &PartialState::node_key(None, &Nibbles::new()), None)?;
        }
        txn.put(db.dbi(Table::Metadata), PARTIAL_ROOT_KEY, self.root.as_slice())?;
        Ok(())
    }

    /// Deletes every stored node of `owner`'s storage trie.
    fn delete_owner(txn: &mut Txn<Rw>, db: &Db, owner: B256) -> Result<(), TrieError> {
        let dbi = db.dbi(Table::TigerNodes);
        let prefix = [&[1u8][..], owner.as_slice()].concat();
        let mut keys = Vec::new();
        {
            let mut cursor = txn.cursor(dbi)?;
            let mut entry = cursor.seek(&prefix)?.map(|(k, _)| k.to_vec());
            while let Some(key) = entry.filter(|k| k.starts_with(&prefix)) {
                assert!(keys.len() < MAX_WITNESS_NODES, "storage trie of {owner} has too many nodes");
                keys.push(key);
                entry = cursor.next()?.map(|(k, _)| k.to_vec());
            }
        }
        for key in &keys {
            txn.delete(dbi, key, None)?;
        }
        Ok(())
    }
}

/// One node of a witness walk: referenced by hash, or embedded in its parent.
enum Pending {
    Hash(Option<B256>, Nibbles, B256),
    Inline(Option<B256>, Nibbles, Vec<u8>),
}

/// The partial-state layout: node keys, leaf decoding, witness intake and the stored root.
#[derive(Debug)]
pub struct PartialState;

impl PartialState {
    /// The `TigerNodes` key of the node at `path` of trie `owner`.
    pub fn node_key(owner: Option<B256>, path: &Nibbles) -> Vec<u8> {
        let packed = TrieCodec::packed_key(path);
        match owner {
            None => [&[0u8][..], &packed].concat(),
            Some(owner) => [&[1u8][..], owner.as_slice(), &packed].concat(),
        }
    }

    /// Decodes an account leaf: the account and its storage root.
    pub fn decode_account_leaf(leaf: &[u8]) -> Result<(Account, B256), TrieError> {
        let account = TrieAccount::decode(&mut &leaf[..]).map_err(|e| TrieError::Corrupt(format!("account leaf: {e}")))?;
        let code_hash = (account.code_hash != alloy_trie::KECCAK_EMPTY).then_some(account.code_hash);
        Ok((Account { nonce: account.nonce, balance: account.balance, bytecode_hash: code_hash }, account.storage_root))
    }

    /// Stores what `nodes` (an execution witness: node RLP in any order) reveal of the trie with
    /// root `root`: every node reachable from the root by hash, at its path, and every leaf's value
    /// in the flat tables; `codes` go to `Bytecodes`. Nodes not reachable are ignored, so a witness
    /// for another root changes nothing.
    pub fn absorb_witness(txn: &mut Txn<Rw>, db: &Db, root: B256, nodes: &[Bytes], codes: &[Bytes]) -> Result<(), TrieError> {
        let by_hash: BTreeMap<B256, &Bytes> = nodes.iter().map(|n| (keccak256(n), n)).collect();
        let (table, accounts, storages) = (db.dbi(Table::TigerNodes), db.dbi(Table::HashedAccounts), db.dbi(Table::HashedStorages));
        let mut stack = Vec::with_capacity(64);
        if root != EMPTY_ROOT_HASH {
            stack.push(Pending::Hash(None, Nibbles::new(), root));
        }
        let mut visited = 0usize;
        while let Some(item) = stack.pop() {
            visited += 1;
            if visited > MAX_WITNESS_NODES {
                return Err(TrieError::Corrupt("witness walk exceeds its node bound".into()));
            }
            let (owner, path, rlp) = match item {
                Pending::Hash(owner, path, hash) => {
                    let Some(rlp) = by_hash.get(&hash) else { continue };
                    txn.put(table, &Self::node_key(owner, &path), rlp)?;
                    (owner, path, rlp.to_vec())
                }
                Pending::Inline(owner, path, rlp) => (owner, path, rlp),
            };
            let node = TrieNode::decode(&mut &rlp[..]).map_err(|e| TrieError::Corrupt(format!("witness node: {e}")))?;
            let child = |path: Nibbles, child: &alloy_trie::nodes::RlpNode| match child.as_hash() {
                Some(hash) => Pending::Hash(owner, path, hash),
                None => Pending::Inline(owner, path, child.as_slice().to_vec()),
            };
            match node {
                TrieNode::EmptyRoot => {}
                TrieNode::Branch(branch) => {
                    let mut items = branch.stack.iter();
                    for nibble in 0..16u8 {
                        if branch.state_mask.is_bit_set(nibble) {
                            let mut p = path;
                            p.push(nibble);
                            stack.push(child(p, items.next().expect("one item per set bit")));
                        }
                    }
                }
                TrieNode::Extension(ext) => stack.push(child(path.join(&ext.key), &ext.child)),
                TrieNode::Leaf(leaf) => {
                    let full = path.join(&leaf.key);
                    if full.len() != 64 {
                        return Err(TrieError::Corrupt(format!("witness leaf at {full:?} is not a 32-byte key")));
                    }
                    let key = B256::from_slice(&full.pack());
                    match owner {
                        None => {
                            let (account, storage_root) = Self::decode_account_leaf(&leaf.value)?;
                            txn.put(accounts, key.as_slice(), &account.to_bytes())?;
                            if storage_root != EMPTY_ROOT_HASH {
                                stack.push(Pending::Hash(Some(key), Nibbles::new(), storage_root));
                            }
                        }
                        Some(owner) => {
                            let value = decode_storage_leaf(&leaf.value)?;
                            let entry = StorageEntry { key, value }.to_bytes();
                            let mut cursor = txn.cursor_rw(storages)?;
                            if cursor.seek_dup(owner.as_slice(), key.as_slice())?.is_some_and(|(_, v)| v.starts_with(key.as_slice())) {
                                cursor.delete_current()?;
                            }
                            drop(cursor);
                            txn.put(storages, owner.as_slice(), &entry)?;
                        }
                    }
                }
            }
        }
        for code in codes {
            txn.put(db.dbi(Table::Bytecodes), keccak256(code).as_slice(), &BytecodeCodec::encode(code))?;
        }
        Ok(())
    }

    /// The stored tip root of a partial datadir, `None` for a full one.
    pub fn root<K>(txn: &Txn<K>, db: &Db) -> Result<Option<B256>, TrieError> {
        Ok(txn.get(db.dbi(Table::Metadata), PARTIAL_ROOT_KEY)?.map(B256::from_slice))
    }
}
