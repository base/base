//! State roots, storage roots, trie updates and proofs over the datadir's tables with hashed
//! state changes layered on top, as reth's `StateRoot`, `StorageRoot` and `Proof` compute them.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_trie::{
    BranchNodeCompact, EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieMask,
    nodes::{BranchNode, RlpNode},
    proof::ProofRetainer,
};
use base_tiger_store::{Account, Datadir, Db, Ro, Txn};

use crate::{
    AccountTrieCursor, HashedAccountCursor, HashedCursor, HashedState, HashedStorage, HashedStorageCursor, LiveSource, MemState,
    OverlayCursor, PrefixSet, StorageTrieCursor, StorageTrieUpdates, TrieCursor, TrieElement, TrieError, TrieNodeIter,
    TrieOverlayCursor, TrieUpdates, TrieWalker, WorkerPool, account_leaf, nibble_keys, storage_leaf,
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
    pub mem: Option<&'t Arc<MemState>>,
}

/// An account overlay entry as the hashed cursor reads it (`None` deletes).
const fn account_value(account: &Option<Account>) -> Option<Account> {
    *account
}

/// A slot overlay entry as the hashed cursor reads it (zero removes).
fn slot_value(value: &U256) -> Option<U256> {
    (!value.is_zero()).then_some(*value)
}

/// The hashed account cursor of a view: the table, the in-memory layer, the view's state.
type AccountCursor<'t, K> = OverlayCursor<'t, OverlayCursor<'t, HashedAccountCursor<'t, K>, Option<Account>>, Option<Account>>;

/// The hashed storage cursor of a view.
type StorageCursor<'t, K> = OverlayCursor<'t, OverlayCursor<'t, HashedStorageCursor<'t, K>, U256>, U256>;

impl<'t, K> TrieView<'t, K> {
    /// A view of `txn` with `state` on top.
    pub const fn new(txn: &'t Txn<K>, db: &'t Db, state: &'t HashedState) -> Self {
        Self { txn, db, state, mem: None }
    }

    /// This view with the in-memory layer `mem` between the tables and its state.
    pub const fn with_mem(self, mem: Option<&'t Arc<MemState>>) -> Self {
        Self { mem, ..self }
    }

    fn account_cursor(&self) -> Result<AccountCursor<'t, K>, TrieError> {
        let mem = self.mem.map(|m| &m.hashed.accounts);
        let base = OverlayCursor::new(HashedAccountCursor::new(self.txn, self.db)?, false, mem, account_value);
        Ok(OverlayCursor::new(base, false, Some(&self.state.accounts), account_value))
    }

    fn storage_cursor(&self, address: B256) -> Result<StorageCursor<'t, K>, TrieError> {
        let mem = self.mem.and_then(|m| m.hashed.storages.get(&address));
        let base = OverlayCursor::new(
            HashedStorageCursor::new(self.txn, self.db, address)?,
            mem.is_some_and(|s| s.wiped),
            mem.map(|s| &s.slots),
            slot_value,
        );
        let changes = self.state.storages.get(&address);
        Ok(OverlayCursor::new(base, changes.is_some_and(|s| s.wiped), changes.map(|s| &s.slots), slot_value))
    }

    /// The trie nodes of `address`'s storage: the stored ones (unless a wipe in memory or in the
    /// view's state dropped them) with the in-memory changes on top.
    fn storage_trie_cursor(&self, address: B256, wiped: bool) -> Result<TrieOverlayCursor<'t, StorageTrieCursor<'t, K>>, TrieError> {
        let mem = self.mem.and_then(|m| m.trie.storage_tries.get(&address)).filter(|_| !wiped);
        let empty = wiped || mem.is_some_and(|m| m.wiped);
        Ok(TrieOverlayCursor::new(StorageTrieCursor::new(self.txn, self.db, address, empty)?, mem.map(|m| &m.nodes)))
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

    /// The storage root of `address` with the node changes that move its stored trie to it, as
    /// [`Self::state_root`] computes them for an account whose storage changed.
    pub fn storage_root_updates(&self, address: B256) -> Result<(B256, StorageTrieUpdates), TrieError> {
        let (root, updates, _) = self.storage_root_inner(address, true, &[])?;
        Ok((root, updates))
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
        let mem = self.mem.map(|m| &m.trie.account_nodes);
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

/// Most threads a parallel state root uses: one per account-trie partition.
const MAX_ROOT_THREADS: usize = 16;

/// The storage roots and storage trie changes of accounts whose storage changed, by hashed
/// address, as [`TrieView::state_root`] computes them with `retain`.
type StorageRoots = HashMap<B256, (B256, StorageTrieUpdates)>;

/// One account-trie partition's part of a parallel state root: the subtree of the keys starting
/// with one nibble.
#[derive(Debug, Default)]
struct Partition {
    /// The reference (inline node or hash) to the subtree's top node, `None` when it holds no keys.
    node: Option<RlpNode>,
    /// Branch nodes the hash builder stored, and stored paths the walker found stale.
    updated: Vec<(Nibbles, BranchNodeCompact)>,
    removed: Vec<Nibbles>,
    /// The changed-storage accounts that are leaves of the subtree.
    storage_leaves: Vec<B256>,
}

/// A trie cursor that sees only the nodes below one nibble of the root.
struct NibbleTrieCursor<C> {
    inner: C,
    nibble: u8,
}

impl<C> NibbleTrieCursor<C> {
    fn keep(&self, entry: Option<(Nibbles, BranchNodeCompact)>) -> Option<(Nibbles, BranchNodeCompact)> {
        entry.filter(|(path, _)| path.first() == Some(self.nibble))
    }
}

impl<C: TrieCursor> TrieCursor for NibbleTrieCursor<C> {
    fn seek_exact(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        if key.first() != Some(self.nibble) {
            return Ok(None);
        }
        let entry = self.inner.seek_exact(key)?;
        Ok(self.keep(entry))
    }

    fn seek(&mut self, key: Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let start = Nibbles::from_nibbles([self.nibble]);
        if key.first().is_some_and(|first| first > self.nibble) {
            return Ok(None);
        }
        let entry = self.inner.seek(key.max(start))?;
        Ok(self.keep(entry))
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let entry = self.inner.next()?;
        Ok(self.keep(entry))
    }

    fn current(&mut self) -> Result<Option<Nibbles>, TrieError> {
        self.inner.current()
    }
}

/// A hashed cursor that sees only the keys whose first nibble is `nibble`.
struct NibbleHashedCursor<H> {
    inner: H,
    nibble: u8,
}

impl<H: HashedCursor> NibbleHashedCursor<H> {
    fn keep(&self, entry: Option<(B256, H::Value)>) -> Option<(B256, H::Value)> {
        entry.filter(|(key, _)| key[0] >> 4 == self.nibble)
    }
}

impl<H: HashedCursor> HashedCursor for NibbleHashedCursor<H> {
    type Value = H::Value;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, H::Value)>, TrieError> {
        let mut start = B256::ZERO;
        start[0] = self.nibble << 4;
        if key[0] >> 4 > self.nibble {
            return Ok(None);
        }
        let entry = self.inner.seek(key.max(start))?;
        Ok(self.keep(entry))
    }

    fn next(&mut self) -> Result<Option<(B256, H::Value)>, TrieError> {
        let entry = self.inner.next()?;
        Ok(self.keep(entry))
    }
}

impl<'t> TrieView<'t, Ro> {
    /// [`Self::state_root`] with `retain`, computed on worker threads, with the same root and the
    /// same trie updates. The account trie is walked as 16 independent subtrees (one per first
    /// nibble) in parallel, each first hashing the changed storage tries of its accounts; their
    /// top nodes make the root branch. Each worker reads through its own read transaction, which must see the same state
    /// as this view's: the same snapshot, or with an in-memory layer, any snapshot whose tip the
    /// layer covers (the datadir only gains blocks it merged). When one does not, or the trie has
    /// fewer than two subtrees, the root is computed here on one thread.
    pub fn state_root_parallel(&self) -> Result<(B256, TrieUpdates), TrieError> {
        let threads = std::thread::available_parallelism().map_or(1, usize::from).min(MAX_ROOT_THREADS);
        if threads < 2 {
            return self.state_root(true);
        }
        // A subtree's walk waits only for its own storage roots, not for every subtree's.
        let Some(results) = self.parallel(threads, 16, |view, nibble| view.partition_with_storage(nibble as u8))? else {
            return self.state_root(true);
        };
        let mut roots = StorageRoots::new();
        let mut partitions = Vec::with_capacity(16);
        for (partition, partition_roots) in results {
            roots.extend(partition_roots);
            partitions.push(partition);
        }
        let mut state_mask = TrieMask::default();
        let mut stack = Vec::with_capacity(16);
        for (nibble, partition) in partitions.iter().enumerate() {
            if let Some(node) = &partition.node {
                state_mask.set_bit(nibble as u8);
                stack.push(node.clone());
            }
        }
        // A root that is not a branch (at most one subtree) is the subtree's own top node.
        if stack.len() < 2 {
            return self.state_root(true);
        }
        let root = keccak256(alloy_rlp::encode(BranchNode { stack, state_mask }));
        let mut updates = TrieUpdates::default();
        for partition in partitions {
            for address in partition.storage_leaves {
                let (_, storage) = roots.remove(&address).expect("every changed storage has a root");
                updates.storage_tries.insert(address, storage);
            }
            TrieUpdates::merge_nodes(&mut updates.account_nodes, partition.updated, partition.removed);
        }
        for (address, storage) in &self.state.storages {
            if storage.wiped {
                updates.storage_tries.entry(*address).or_insert_with(|| StorageTrieUpdates { wiped: true, ..Default::default() });
            }
        }
        Ok((root, updates))
    }

    /// Runs `job(view, index)` for every index below `jobs` on `threads` threads, each over a
    /// view through its own read transaction; `None` when one of them sees another state. The
    /// calling thread works too, with idle [`WorkerPool`] threads.
    fn parallel<R: Send + 'static>(
        &self,
        threads: usize,
        jobs: usize,
        job: impl Fn(&TrieView<'_, Ro>, usize) -> Result<R, TrieError> + Send + Sync + 'static,
    ) -> Result<Option<Vec<R>>, TrieError> {
        let (db, mem, snapshot) = (self.db.clone(), self.mem.cloned(), self.txn.id());
        let state = Arc::new(self.state.clone());
        let open = {
            let (db, mem) = (db.clone(), mem.clone());
            move || -> Result<Option<Txn<Ro>>, TrieError> {
                let txn = db.read()?;
                // The same state: the same snapshot, or one whose tip the in-memory layer covers.
                let same = match &mem {
                    Some(mem) => Datadir::tip_in(&txn, &db)?.is_some_and(|tip| tip <= mem.head),
                    None => txn.id() == snapshot,
                };
                Ok(same.then_some(txn))
            }
        };
        let results = WorkerPool::global().run(jobs, threads - 1, open, move |txn: &mut Option<Txn<Ro>>, index| {
            let Some(txn) = txn else { return Ok(None) };
            let view = TrieView { txn, db: &db, state: &state, mem: mem.as_ref() };
            job(&view, index).map(Some)
        })?;
        Ok(results.into_iter().collect())
    }

    /// The storage roots of the changed storages whose keys start with `nibble`, then that
    /// account subtree's walk over them (see [`Self::walk_partition`]).
    fn partition_with_storage(&self, nibble: u8) -> Result<(Partition, StorageRoots), TrieError> {
        let mut roots = StorageRoots::new();
        for address in self.state.storages.keys().filter(|key| key[0] >> 4 == nibble) {
            roots.insert(*address, self.storage_root_updates(*address)?);
        }
        let hashes = roots.iter().map(|(address, (root, _))| (*address, *root)).collect();
        Ok((self.walk_partition(nibble, &hashes)?, roots))
    }

    /// Walks the subtree of the account keys starting with `nibble` as [`Self::state_root`] walks
    /// the whole trie, taking changed accounts' storage roots from `roots`. A placeholder branch
    /// at a neighboring nibble makes the hash builder treat the subtree as a child of a root
    /// branch, so it builds, stores and references the subtree's nodes exactly as in a walk of
    /// the whole trie; the subtree's top node is the one it retains at the nibble's path.
    fn walk_partition(&self, nibble: u8, roots: &HashMap<B256, B256>) -> Result<Partition, TrieError> {
        let prefix = Nibbles::from_nibbles([nibble]);
        let mine = |key: &&B256| key[0] >> 4 == nibble;
        let changed = self.state.accounts.keys().chain(self.state.storages.keys()).filter(mine);
        let mem = self.mem.map(|m| &m.trie.account_nodes);
        let cursor = NibbleTrieCursor { inner: TrieOverlayCursor::new(AccountTrieCursor::new(self.txn, self.db)?, mem), nibble };
        let walker = TrieWalker::new(cursor, PrefixSet::new(nibble_keys(changed)), true)?;
        let mut builder = HashBuilder::default().with_updates(true).with_proof_retainer(ProofRetainer::new(vec![prefix]));
        let placeholder = |nibble: u8| Nibbles::from_nibbles([nibble]);
        if nibble == 15 {
            builder.add_branch(placeholder(14), B256::ZERO, false);
        }
        let mut iter = TrieNodeIter::new(walker, NibbleHashedCursor { inner: self.account_cursor()?, nibble });
        let mut partition = Partition::default();
        let mut empty = true;
        while let Some(element) = iter.try_next()? {
            empty = false;
            match element {
                TrieElement::Branch(key, hash, stored) => {
                    if key == prefix {
                        partition.node = Some(RlpNode::word_rlp(&hash));
                    }
                    builder.add_branch(key, hash, stored);
                }
                TrieElement::Leaf(address, account) => {
                    let storage_root = match roots.get(&address) {
                        Some(root) => {
                            partition.storage_leaves.push(address);
                            *root
                        }
                        None => self.storage_root_inner(address, false, &[])?.0,
                    };
                    builder.add_leaf(Nibbles::unpack(address), &account_leaf(&account, storage_root));
                }
            }
        }
        partition.removed = iter.walker.removed_keys.take().unwrap_or_default();
        // An emptied subtree adds no node, only the removal of its stored ones.
        if empty {
            return Ok(partition);
        }
        if nibble < 15 {
            builder.add_branch(placeholder(nibble + 1), B256::ZERO, false);
        }
        builder.root();
        if partition.node.is_none() {
            let top = builder.take_proof_nodes().get(&prefix).map(|rlp| RlpNode::from_rlp(rlp));
            partition.node = Some(top.ok_or_else(|| TrieError::Corrupt(format!("no top node in account subtree {nibble:x}")))?);
        }
        let (_, updated) = builder.split();
        partition.updated = updated.into_iter().filter(|(path, _)| path.first() == Some(nibble)).collect();
        Ok(partition)
    }
}

/// The smallest key under the nibble path `prefix`.
fn key_floor(prefix: &Nibbles) -> B256 {
    let mut key = [0u8; 32];
    for (i, nibble) in prefix.to_vec().into_iter().enumerate() {
        key[i / 2] |= if i % 2 == 0 { nibble << 4 } else { nibble };
    }
    B256::from(key)
}

/// A view's state as a [`crate::LiveTrie`] source: its stored rows and flat leaves with the
/// in-memory layer and the view's changes on top.
impl<K> LiveSource for TrieView<'_, K> {
    fn branch(&mut self, owner: Option<B256>, path: &Nibbles) -> Result<Option<BranchNodeCompact>, TrieError> {
        Ok(self.first_branch_under(owner, path)?.filter(|(p, _)| p == path).map(|(_, node)| node))
    }

    fn first_branch_under(&mut self, owner: Option<B256>, prefix: &Nibbles) -> Result<Option<(Nibbles, BranchNodeCompact)>, TrieError> {
        let found = match owner {
            None => {
                let mem = self.mem.map(|m| &m.trie.account_nodes);
                TrieOverlayCursor::new(AccountTrieCursor::new(self.txn, self.db)?, mem).seek(*prefix)?
            }
            Some(address) => {
                let wiped = self.state.storages.get(&address).is_some_and(|s| s.wiped);
                self.storage_trie_cursor(address, wiped)?.seek(*prefix)?
            }
        };
        Ok(found.filter(|(path, _)| path.starts_with(prefix)))
    }

    fn leaves(&mut self, owner: Option<B256>, prefix: &Nibbles, out: &mut Vec<(B256, Vec<u8>)>) -> Result<(), TrieError> {
        let under = |key: &B256| Nibbles::unpack(*key).starts_with(prefix);
        match owner {
            None => {
                let mut cursor = self.account_cursor()?;
                let mut entry = cursor.seek(key_floor(prefix))?;
                while let Some((key, account)) = entry.filter(|(k, _)| under(k)) {
                    out.push((key, account_leaf(&account, self.storage_root(key)?)));
                    entry = cursor.next()?;
                }
            }
            Some(address) => {
                let mut cursor = self.storage_cursor(address)?;
                let mut entry = cursor.seek(key_floor(prefix))?;
                while let Some((slot, value)) = entry.filter(|(k, _)| under(k)) {
                    out.push((slot, storage_leaf(value)));
                    entry = cursor.next()?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::keccak256;
    use alloy_trie::proof::verify_proof;
    use base_tiger_store::{Table, TrieCodec};

    use super::*;
    use crate::{LiveState, MemState, Sources, StateWriter};

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

    /// Checks that `live` trie updates do what the walker's `walker` do over `txn`'s stored trie:
    /// the same storage tries with the same wipes, and node changes that are the walker's minus
    /// rewrites of rows that do not change (the walker rewrites every stored node it walks
    /// through, the live trie only the ones that change).
    fn assert_same_effect<K>(txn: &Txn<K>, db: &Db, live: &TrieUpdates, walker: &TrieUpdates, at: &str) {
        let check = |owner: Option<B256>,
                     live: &BTreeMap<Nibbles, Option<alloy_trie::BranchNodeCompact>>,
                     walker: &BTreeMap<Nibbles, Option<alloy_trie::BranchNodeCompact>>,
                     wiped: bool| {
            for (path, node) in live {
                assert_eq!(walker.get(path), Some(node), "{at}: {owner:?} row {path:?} only in the live updates");
            }
            for (path, node) in walker {
                if live.contains_key(path) {
                    continue;
                }
                let empty = HashedState::default();
                let mut source = TrieView::new(txn, db, &empty);
                let stored = if wiped { None } else { LiveSource::branch(&mut source, owner, path).unwrap() };
                assert_eq!(node.as_ref(), stored.as_ref(), "{at}: {owner:?} row {path:?} changed only by the walker");
            }
        };
        check(None, &live.account_nodes, &walker.account_nodes, false);
        assert_eq!(live.storage_tries.keys().collect::<Vec<_>>(), walker.storage_tries.keys().collect::<Vec<_>>(), "{at}: storage tries");
        for (address, trie) in &walker.storage_tries {
            let ours = &live.storage_tries[address];
            assert_eq!(ours.wiped, trie.wiped, "{at}: wipe of {address}");
            check(Some(*address), &ours.nodes, &trie.nodes, trie.wiped);
        }
    }

    #[test]
    fn incremental_roots_nodes_and_proofs_match_a_from_scratch_model() {
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let (dir, db) = temp_db(&format!("model-{seed}"));
            let mut model = Model::new();
            let mut keys: Vec<B256> = Vec::new();
            let mut live = LiveState::default();
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
                let parallel = TrieView::new(&db.read().unwrap(), &db, &state).state_root_parallel().unwrap();
                let mut txn = db.write().unwrap();
                let (root, updates) = TrieView::new(&txn, &db, &state).state_root(true).unwrap();
                assert_eq!(root, model_root(&model), "seed {seed} block {block}");
                // The trie kept in memory across blocks (dropped now and then) gives the same.
                if rng.below(8) == 0 {
                    live = LiveState::default();
                }
                let parent = HashedState::default();
                let mut source = TrieView::new(&txn, &db, &parent);
                let (live_root, live_updates) = live.apply(&mut Sources::Local(&mut source), &state).unwrap();
                assert_eq!(live_root, root, "seed {seed} block {block}: live root");
                assert_same_effect(&txn, &db, &live_updates, &updates, &format!("seed {seed} block {block}"));
                assert_eq!(parallel, (root, updates.clone()), "parallel: seed {seed} block {block}");
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

    /// Sets the stored tip (the `Finish` checkpoint) to `block`.
    fn set_tip(txn: &mut Txn<base_tiger_store::Rw>, db: &Db, block: u64) {
        txn.put(db.dbi(Table::StageCheckpoints), b"Finish", &base_tiger_store::StageCheckpointCodec::encode(block)).unwrap();
    }

    /// A random block of account, slot, wipe and delete changes over `keys`.
    fn random_state(rng: &mut Rng, keys: &mut Vec<B256>) -> HashedState {
        let mut state = HashedState::default();
        for _ in 0..1 + rng.below(60) {
            let key = if !keys.is_empty() && rng.below(3) > 0 {
                keys[rng.below(keys.len() as u64) as usize]
            } else {
                let key = if rng.below(2) == 0 { rng.key(0) } else { keccak256(rng.next().to_be_bytes()) };
                keys.push(key);
                key
            };
            match rng.below(12) {
                0..=4 => state.set_account(key, Account { nonce: rng.below(3), balance: U256::from(1 + rng.below(1000)), bytecode_hash: None }),
                5..=9 => {
                    let value = if rng.below(4) == 0 { U256::ZERO } else { U256::from(rng.below(1 << 20)) };
                    state.set_slot(key, rng.key(1), value);
                }
                10 => state.wipe_storage(key),
                _ => state.delete_account(key),
            }
        }
        state
    }

    #[test]
    fn parallel_root_matches_over_a_memory_layer_while_the_datadir_advances() {
        for seed in 0..8u64 {
            let mut rng = Rng(100 + seed);
            let (dir, db) = temp_db(&format!("parallel-mem-{seed}"));
            let mut keys = Vec::new();
            let mut w = db.write().unwrap();
            let genesis = random_state(&mut rng, &mut keys);
            let (_, updates) = TrieView::new(&w, &db, &genesis).state_root(true).unwrap();
            StateWriter::write_hashed_state(&mut w, &db, &genesis).unwrap();
            StateWriter::write_trie_updates(&mut w, &db, &updates).unwrap();
            set_tip(&mut w, &db, 0);
            w.commit().unwrap();
            let mut mem = Arc::new(MemState::new(0));
            let mut persisted = 0;
            for block in 1..40u64 {
                let state = random_state(&mut rng, &mut keys);
                let ro = db.read().unwrap();
                // The persistence of the layer's oldest blocks lands while the root is computed.
                if block % 5 == 0 {
                    let mut w = db.write().unwrap();
                    StateWriter::write_hashed_state(&mut w, &db, &mem.hashed).unwrap();
                    StateWriter::write_trie_updates(&mut w, &db, &mem.trie).unwrap();
                    set_tip(&mut w, &db, mem.head);
                    w.commit().unwrap();
                    persisted = mem.head;
                }
                let view = TrieView::new(&ro, &db, &state).with_mem(Some(&mem));
                let sequential = view.state_root(true).unwrap();
                assert_eq!(view.state_root_parallel().unwrap(), sequential, "seed {seed} block {block}");
                Arc::make_mut(&mut mem).extend(block, &state, &sequential.1, &[]);
            }
            assert!(persisted > 0);
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
