//! State roots, storage roots, trie updates and proofs over the datadir's tables with hashed
//! state changes layered on top, as reth's `StateRoot`, `StorageRoot` and `Proof` compute them.

use alloy_primitives::{B256, Bytes, U256};
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, proof::ProofRetainer};
use base_tiger_store::{Account, Db, Txn};

use crate::{
    AccountTrieCursor, HashedAccountCursor, HashedCursor, HashedState, HashedStorage, HashedStorageCursor, MemState,
    OverlayCursor, PrefixSet, StorageTrieCursor, StorageTrieUpdates, TrieElement, TrieError, TrieNodeIter,
    TrieOverlayCursor, TrieUpdates, TrieWalker, account_leaf, nibble_keys, storage_leaf,
};

/// An account proof with its storage proofs (`eth_getProof`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProof {
    /// The hashed address.
    pub key: B256,
    /// The account, `None` when absent.
    pub account: Option<Account>,
    /// The account's storage root (empty root when absent).
    pub storage_root: B256,
    /// Account trie nodes from the root along the key's path.
    pub proof: Vec<Bytes>,
    /// Storage proofs, in request order.
    pub storage: Vec<StorageProof>,
}

/// One storage slot proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageProof {
    /// The hashed slot.
    pub slot: B256,
    /// The value (zero when unset).
    pub value: U256,
    /// Storage trie nodes from the storage root along the slot's path.
    pub proof: Vec<Bytes>,
}

/// Trie computations over one transaction's view of the tables plus in-memory changes.
#[derive(Debug)]
pub struct TrieView<'t, K> {
    /// The transaction.
    pub txn: &'t Txn<K>,
    /// The table handles.
    pub db: &'t Db,
    /// Hashed state changes over the tables.
    pub state: &'t HashedState,
    /// The in-memory canonical blocks between the tables and `state`.
    pub mem: Option<&'t MemState>,
}

/// Sorted overlay entries of one account's storage changes (zero removes).
fn slot_overlay(changes: Option<&HashedStorage>) -> Vec<(B256, Option<U256>)> {
    changes.map(|s| s.slots.iter().map(|(k, v)| (*k, (!v.is_zero()).then_some(*v))).collect()).unwrap_or_default()
}

impl<'t, K> TrieView<'t, K> {
    /// A view of `txn` with `state` on top.
    pub const fn new(txn: &'t Txn<K>, db: &'t Db, state: &'t HashedState) -> Self {
        Self { txn, db, state, mem: None }
    }

    /// This view with the in-memory layer `mem` between the tables and its state.
    pub const fn with_mem(self, mem: Option<&'t MemState>) -> Self {
        Self { mem, ..self }
    }

    fn account_cursor(&self) -> Result<OverlayCursor<OverlayCursor<HashedAccountCursor<'t, K>>>, TrieError> {
        let mem = self.mem.map(|m| m.hashed.accounts.iter().map(|(k, a)| (*k, *a)).collect()).unwrap_or_default();
        let base = OverlayCursor::new(HashedAccountCursor::new(self.txn, self.db)?, false, mem);
        let overlay = self.state.accounts.iter().map(|(k, a)| (*k, *a)).collect();
        Ok(OverlayCursor::new(base, false, overlay))
    }

    fn storage_cursor(&self, address: B256) -> Result<OverlayCursor<OverlayCursor<HashedStorageCursor<'t, K>>>, TrieError> {
        let mem = self.mem.and_then(|m| m.hashed.storages.get(&address));
        let base = OverlayCursor::new(
            HashedStorageCursor::new(self.txn, self.db, address)?,
            mem.is_some_and(|s| s.wiped),
            slot_overlay(mem),
        );
        let changes = self.state.storages.get(&address);
        Ok(OverlayCursor::new(base, changes.is_some_and(|s| s.wiped), slot_overlay(changes)))
    }

    /// The trie nodes of `address`'s storage: the stored ones (unless a wipe in memory or in the
    /// view's state dropped them) with the in-memory changes on top.
    fn storage_trie_cursor(&self, address: B256, wiped: bool) -> Result<TrieOverlayCursor<StorageTrieCursor<'t, K>>, TrieError> {
        let mem = self.mem.and_then(|m| m.trie.storage_tries.get(&address)).filter(|_| !wiped);
        let empty = wiped || mem.is_some_and(|m| m.wiped);
        let overlay = mem.map(|m| m.nodes.iter().map(|(p, n)| (*p, n.clone())).collect()).unwrap_or_default();
        Ok(TrieOverlayCursor::new(StorageTrieCursor::new(self.txn, self.db, address, empty)?, overlay))
    }

    fn storage_prefix_set(&self, address: B256, extra: &[B256]) -> PrefixSet {
        match self.state.storages.get(&address) {
            Some(HashedStorage { wiped: true, .. }) => PrefixSet::all(),
            Some(storage) => PrefixSet::new(nibble_keys(storage.slots.keys().chain(extra))),
            None => PrefixSet::new(nibble_keys(extra)),
        }
    }

    /// The storage root of `address` with its updated nodes, and storage proofs for `targets`.
    fn storage_root_inner(
        &self,
        address: B256,
        retain: bool,
        targets: &[B256],
    ) -> Result<(B256, StorageTrieUpdates, Vec<StorageProof>), TrieError> {
        let wiped = self.state.storages.get(&address).is_some_and(|s| s.wiped);
        let mut updates = StorageTrieUpdates { wiped, ..Default::default() };
        let mut cursor = self.storage_cursor(address)?;
        let prefix_set = self.storage_prefix_set(address, targets);
        if cursor.seek(B256::ZERO)?.is_none() && (!retain || prefix_set.is_empty()) && targets.is_empty() {
            return Ok((EMPTY_ROOT_HASH, updates, Vec::new()));
        }
        let trie_cursor = self.storage_trie_cursor(address, wiped)?;
        let walker = TrieWalker::new(trie_cursor, prefix_set, retain)?;
        let mut builder = HashBuilder::default().with_updates(retain);
        let target_paths = nibble_keys(targets);
        if !targets.is_empty() {
            builder = builder.with_proof_retainer(ProofRetainer::new(target_paths.clone()));
        }
        let mut iter = TrieNodeIter::new(walker, cursor);
        let mut values = Vec::with_capacity(targets.len());
        while let Some(element) = iter.try_next()? {
            match element {
                TrieElement::Branch(key, hash, stored) => builder.add_branch(key, hash, stored),
                TrieElement::Leaf(slot, value) => {
                    if targets.contains(&slot) {
                        values.push((slot, value));
                    }
                    builder.add_leaf(Nibbles::unpack(slot), &storage_leaf(value));
                }
            }
        }
        let root = builder.root();
        let proofs = if targets.is_empty() {
            Vec::new()
        } else {
            let nodes = builder.take_proof_nodes();
            targets
                .iter()
                .map(|slot| {
                    let path = Nibbles::unpack(*slot);
                    let value = values.iter().find(|(s, _)| s == slot).map_or(U256::ZERO, |(_, v)| *v);
                    let proof = if root == EMPTY_ROOT_HASH {
                        Vec::new()
                    } else {
                        nodes.matching_nodes_sorted(&path).into_iter().map(|(_, node)| node).collect()
                    };
                    StorageProof { slot: *slot, value, proof }
                })
                .collect()
        };
        if retain {
            let (_, updated) = builder.split();
            let removed = iter.walker.removed_keys.take().unwrap_or_default();
            TrieUpdates::merge_nodes(&mut updates.nodes, updated, removed);
        }
        Ok((root, updates, proofs))
    }

    /// The storage root of `address`.
    pub fn storage_root(&self, address: B256) -> Result<B256, TrieError> {
        Ok(self.storage_root_inner(address, false, &[])?.0)
    }

    /// The state root, and the trie node changes that move the stored trie to it when `retain`.
    pub fn state_root(&self, retain: bool) -> Result<(B256, TrieUpdates), TrieError> {
        let (root, updates, _) = self.walk_accounts(retain, None)?;
        Ok((root, updates))
    }

    /// The account proof of `key` with storage proofs for `slots`.
    pub fn proof(&self, key: B256, slots: &[B256]) -> Result<AccountProof, TrieError> {
        let (_, _, proof) = self.walk_accounts(false, Some((key, slots)))?;
        Ok(proof.expect("a proof was requested"))
    }

    fn walk_accounts(
        &self,
        retain: bool,
        target: Option<(B256, &[B256])>,
    ) -> Result<(B256, TrieUpdates, Option<AccountProof>), TrieError> {
        let mut updates = TrieUpdates::default();
        let mut changed: Vec<&B256> = self.state.accounts.keys().chain(self.state.storages.keys()).collect();
        if let Some((key, _)) = &target {
            changed.push(key);
        }
        let mem = self.mem.map(|m| m.trie.account_nodes.iter().map(|(p, n)| (*p, n.clone())).collect()).unwrap_or_default();
        let cursor = TrieOverlayCursor::new(AccountTrieCursor::new(self.txn, self.db)?, mem);
        let walker = TrieWalker::new(cursor, PrefixSet::new(nibble_keys(changed)), retain)?;
        let mut builder = HashBuilder::default().with_updates(retain);
        if let Some((key, _)) = &target {
            builder = builder.with_proof_retainer(ProofRetainer::new(vec![Nibbles::unpack(*key)]));
        }
        let mut iter = TrieNodeIter::new(walker, self.account_cursor()?);
        let mut proof = target.map(|(key, _)| AccountProof {
            key,
            account: None,
            storage_root: EMPTY_ROOT_HASH,
            proof: Vec::new(),
            storage: Vec::new(),
        });
        while let Some(element) = iter.try_next()? {
            match element {
                TrieElement::Branch(key, hash, stored) => builder.add_branch(key, hash, stored),
                TrieElement::Leaf(address, account) => {
                    let slots: &[B256] = match target {
                        Some((key, slots)) if key == address => slots,
                        _ => &[],
                    };
                    let (storage_root, storage_updates, storage_proofs) =
                        self.storage_root_inner(address, retain && self.state.storages.contains_key(&address), slots)?;
                    if retain && self.state.storages.contains_key(&address) {
                        updates.storage_tries.insert(address, storage_updates);
                    }
                    if let Some(proof) = proof.as_mut().filter(|p| p.key == address) {
                        proof.account = Some(account);
                        proof.storage_root = storage_root;
                        proof.storage = storage_proofs;
                    }
                    builder.add_leaf(Nibbles::unpack(address), &account_leaf(&account, storage_root));
                }
            }
        }
        let root = builder.root();
        if let Some(proof) = proof.as_mut() {
            let path = Nibbles::unpack(proof.key);
            if root != EMPTY_ROOT_HASH {
                proof.proof = builder.take_proof_nodes().matching_nodes_sorted(&path).into_iter().map(|(_, n)| n).collect();
            }
            if proof.account.is_none() {
                proof.storage = target
                    .unwrap()
                    .1
                    .iter()
                    .map(|slot| StorageProof { slot: *slot, value: U256::ZERO, proof: Vec::new() })
                    .collect();
            }
        }
        if retain {
            // Storage tries of accounts that end deleted or wiped: drop every stored node.
            for (address, storage) in &self.state.storages {
                if storage.wiped {
                    updates.storage_tries.entry(*address).or_insert_with(|| StorageTrieUpdates { wiped: true, ..Default::default() });
                }
            }
            let (_, updated) = builder.split();
            let removed = iter.walker.removed_keys.take().unwrap_or_default();
            TrieUpdates::merge_nodes(&mut updates.account_nodes, updated, removed);
        }
        Ok((root, updates, proof))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::keccak256;
    use alloy_trie::proof::verify_proof;
    use base_tiger_store::{Table, TrieCodec};

    use super::*;
    use crate::StateWriter;

    /// splitmix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        /// A key sharing a random-length prefix with one of a few fixed prefixes, so tries get
        /// deep shared paths, extensions and branch collapses.
        fn key(&mut self, salt: u8) -> B256 {
            let mut key = keccak256([salt, self.below(4) as u8]).0;
            let shared = self.below(12) as usize;
            let tail = keccak256(self.next().to_be_bytes()).0;
            for i in shared..64 {
                let nibble = if i % 2 == 0 { tail[i / 2] >> 4 } else { tail[i / 2] & 0xf };
                key[i / 2] = if i % 2 == 0 { (key[i / 2] & 0x0f) | (nibble << 4) } else { (key[i / 2] & 0xf0) | nibble };
            }
            B256::from(key)
        }
    }

    /// Proof nodes without the embedded ones (shorter than 32 bytes, except the root), which
    /// the verifier finds inside their parents.
    fn hashed_nodes(proof: &[Bytes]) -> Vec<Bytes> {
        proof.iter().enumerate().filter(|(i, n)| *i == 0 || n.len() >= 32).map(|(_, n)| n.clone()).collect()
    }

    type Model = BTreeMap<B256, (Account, BTreeMap<B256, U256>)>;

    fn model_root(model: &Model) -> B256 {
        let mut builder = HashBuilder::default();
        for (key, (account, storage)) in model {
            let mut storage_builder = HashBuilder::default();
            for (slot, value) in storage {
                storage_builder.add_leaf(Nibbles::unpack(*slot), &storage_leaf(*value));
            }
            builder.add_leaf(Nibbles::unpack(*key), &account_leaf(account, storage_builder.root()));
        }
        builder.root()
    }

    fn temp_db(name: &str) -> (std::path::PathBuf, Db) {
        let dir = std::env::temp_dir().join(format!("tiger-trie-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir).unwrap();
        (dir, db)
    }

    /// Every stored node, read back from the tables.
    fn stored_nodes(txn: &Txn<base_tiger_store::Rw>, db: &Db) -> BTreeMap<(Option<B256>, Nibbles), alloy_trie::BranchNodeCompact> {
        let mut out = BTreeMap::new();
        let mut cursor = txn.cursor(db.dbi(Table::AccountsTrie)).unwrap();
        let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        while let Some((k, v)) = entry {
            out.insert((None, TrieCodec::unpack_key(&k).unwrap()), TrieCodec::decode_node(&v).unwrap());
            entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        }
        let mut cursor = txn.cursor(db.dbi(Table::StoragesTrie)).unwrap();
        let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        while let Some((k, v)) = entry {
            let (path, node) = TrieCodec::decode_storage_entry(&v).unwrap();
            out.insert((Some(B256::from_slice(&k)), path), node);
            entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        }
        out
    }

    /// The nodes a from-scratch build of `model` stores, computed over an empty database.
    fn rebuilt_nodes(model: &Model) -> BTreeMap<(Option<B256>, Nibbles), alloy_trie::BranchNodeCompact> {
        let (dir, db) = temp_db("rebuild");
        let mut state = HashedState::default();
        for (key, (account, storage)) in model {
            state.set_account(*key, *account);
            for (slot, value) in storage {
                state.set_slot(*key, *slot, *value);
            }
        }
        let mut txn = db.write().unwrap();
        let (_, updates) = TrieView::new(&txn, &db, &state).state_root(true).unwrap();
        StateWriter::write_trie_updates(&mut txn, &db, &updates).unwrap();
        let nodes = stored_nodes(&txn, &db);
        drop(txn);
        let _ = std::fs::remove_dir_all(dir);
        nodes
    }

    #[test]
    fn incremental_roots_nodes_and_proofs_match_a_from_scratch_model() {
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let (dir, db) = temp_db(&format!("model-{seed}"));
            let mut model = Model::new();
            let mut keys: Vec<B256> = Vec::new();
            for block in 0..30 {
                let mut state = HashedState::default();
                let mut next = model.clone();
                let ops = 1 + rng.below(if block % 7 == 6 { 120 } else { 25 });
                for _ in 0..ops {
                    let existing: Vec<B256> = next.keys().copied().collect();
                    let pick = |rng: &mut Rng, keys: &mut Vec<B256>| -> B256 {
                        if !existing.is_empty() && rng.below(3) > 0 {
                            existing[rng.below(existing.len() as u64) as usize]
                        } else if !keys.is_empty() && rng.below(2) == 0 {
                            keys[rng.below(keys.len() as u64) as usize]
                        } else {
                            let key = rng.key(0);
                            keys.push(key);
                            key
                        }
                    };
                    let key = pick(&mut rng, &mut keys);
                    match rng.below(10) {
                        0..=3 => {
                            let account = Account {
                                nonce: rng.below(3),
                                balance: U256::from(rng.below(1000) + 1),
                                bytecode_hash: (rng.below(3) == 0).then(|| B256::repeat_byte(7)),
                            };
                            state.set_account(key, account);
                            next.entry(key).or_insert((account, BTreeMap::new())).0 = account;
                        }
                        4..=7 if next.contains_key(&key) => {
                            // Small values make embedded leaves.
                            let slot = rng.key(1);
                            let value = if rng.below(4) == 0 { U256::ZERO } else { U256::from(rng.below(5)) };
                            let slot = match next[&key].1.keys().nth(rng.below(8) as usize) {
                                Some(existing) if rng.below(2) == 0 => *existing,
                                _ => slot,
                            };
                            state.set_slot(key, slot, value);
                            let storage = &mut next.get_mut(&key).unwrap().1;
                            if value.is_zero() {
                                storage.remove(&slot);
                            } else {
                                storage.insert(slot, value);
                            }
                        }
                        8 if next.contains_key(&key) => {
                            state.wipe_storage(key);
                            next.get_mut(&key).unwrap().1.clear();
                        }
                        9 => {
                            state.delete_account(key);
                            next.remove(&key);
                        }
                        _ => {}
                    }
                }
                model = next;
                let mut txn = db.write().unwrap();
                let (root, updates) = TrieView::new(&txn, &db, &state).state_root(true).unwrap();
                assert_eq!(root, model_root(&model), "seed {seed} block {block}");
                StateWriter::write_hashed_state(&mut txn, &db, &state).unwrap();
                StateWriter::write_trie_updates(&mut txn, &db, &updates).unwrap();
                let empty = HashedState::default();
                assert_eq!(TrieView::new(&txn, &db, &empty).state_root(false).unwrap().0, root);
                if block % 5 == 4 {
                    assert_eq!(stored_nodes(&txn, &db), rebuilt_nodes(&model), "seed {seed} block {block}");
                }
                // Proofs of present, absent and nearby keys verify against the root.
                for _ in 0..4 {
                    let key = if !model.is_empty() && rng.below(2) == 0 {
                        *model.keys().nth(rng.below(model.len() as u64) as usize).unwrap()
                    } else {
                        rng.key(0)
                    };
                    let slots: Vec<B256> = match model.get(&key) {
                        Some((_, storage)) => storage.keys().take(2).copied().chain([rng.key(1)]).collect(),
                        None => vec![rng.key(1)],
                    };
                    let proof = TrieView::new(&txn, &db, &empty).proof(key, &slots).unwrap();
                    let expected = model.get(&key);
                    assert_eq!(proof.account, expected.map(|(a, _)| *a));
                    let leaf = expected.map(|(a, _)| account_leaf(a, proof.storage_root));
                    verify_proof(root, Nibbles::unpack(key), leaf, &hashed_nodes(&proof.proof)).unwrap();
                    for storage in &proof.storage {
                        let value = expected.and_then(|(_, s)| s.get(&storage.slot)).copied().unwrap_or_default();
                        assert_eq!(storage.value, value);
                        if expected.is_some() {
                            let leaf = (!value.is_zero()).then(|| storage_leaf(value));
                            let nodes = hashed_nodes(&storage.proof);
                            verify_proof(proof.storage_root, Nibbles::unpack(storage.slot), leaf, &nodes).unwrap();
                        }
                    }
                }
                txn.commit().unwrap();
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
