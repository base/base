//! The state database: flat hashed state, trie nodes, changesets and history in a base-reth
//! storage v2 datadir, with reads, roots and proofs at any stored block plus in-memory blocks on
//! top.
//!
//! - Flat state is `HashedAccounts` / `HashedStorages` (v2 keeps no plain state); code is
//!   `Bytecodes`.
//! - A block's prior values go to the account and storage changeset static files (plain address
//!   and slot, as reth writes them) and its block number to the `AccountsHistory` /
//!   `StoragesHistory` RocksDB index, so a seek finds the first change after any block.
//! - A storage wipe (or account deletion) also records the whole wiped storage in
//!   `TigerWipedStorage` and `TigerWipes`, which reth's v2 changesets lack, so unwinds and
//!   historical reads restore it exactly.
//! - State at an older block is the current state with the changesets of later blocks reverted
//!   in memory; roots and proofs there run the same trie code over that overlay.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_trie::KECCAK_EMPTY;
use base_tiger_store::{
    Account, AccountBeforeTx, BytecodeCodec, Datadir, Db, History, Reader, RocksTable, Ro, Rw, Segment, StorageBeforeTx,
    StorageEntry, StoreError, Table, Txn, WriteTxn, put_compact_u256,
};

use alloy_rlp::Decodable;

use crate::{
    AccountProof, DbNodes, EMPTY_ROOT, HashedState, HashedStorage, PARTIAL_ROOT_KEY, ProofNodes, Recording, SparseTrie,
    PartialState, StateTries, StateWriter, TrieError, TrieView, hash_key,
};

/// An account's fields as the state database takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccountInfo {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash (`KECCAK_EMPTY` for no code).
    pub code_hash: B256,
}

impl AccountInfo {
    /// reth's stored form (`None` code hash for empty code).
    pub fn to_account(self) -> Account {
        Account { nonce: self.nonce, balance: self.balance, bytecode_hash: (self.code_hash != KECCAK_EMPTY).then_some(self.code_hash) }
    }

    /// From reth's stored form.
    pub fn from_account(account: Account) -> Self {
        Self { nonce: account.nonce, balance: account.balance, code_hash: account.bytecode_hash.unwrap_or(KECCAK_EMPTY) }
    }
}

/// One block's state changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockChanges {
    /// Accounts after the block; `None` deletes the account with its storage. A later entry for
    /// the same address wins.
    pub accounts: Vec<(Address, Option<AccountInfo>)>,
    /// Storage after the block: `(address, slot, value)`, zero clears. A later entry wins.
    pub storage: Vec<(Address, U256, U256)>,
    /// Addresses whose storage was cleared before this block's `storage` entries apply.
    pub wiped: Vec<Address>,
    /// New code by hash.
    pub codes: Vec<(B256, Bytes)>,
}

/// [`BlockChanges`] with duplicates resolved, sorted.
#[derive(Debug, Clone, Default)]
struct Normalized {
    accounts: BTreeMap<Address, Option<AccountInfo>>,
    storage: BTreeMap<(Address, B256), U256>,
    wiped: BTreeSet<Address>,
}

impl BlockChanges {
    fn normalized(&self) -> Normalized {
        let accounts = self.accounts.iter().copied().collect();
        let storage = self.storage.iter().map(|(a, s, v)| ((*a, B256::from(s.to_be_bytes::<32>())), *v)).collect();
        let wiped = self.wiped.iter().copied().collect();
        Normalized { accounts, storage, wiped }
    }

    /// The changes in hashed-key space.
    pub fn hashed(&self) -> HashedState {
        let n = self.normalized();
        let mut state = HashedState::default();
        for address in &n.wiped {
            state.wipe_storage(hash_key(address.as_slice()));
        }
        for ((address, slot), value) in &n.storage {
            state.set_slot(hash_key(address.as_slice()), hash_key(slot.as_slice()), *value);
        }
        for (address, info) in &n.accounts {
            let key = hash_key(address.as_slice());
            match info {
                Some(info) => state.set_account(key, info.to_account()),
                None => state.delete_account(key),
            }
        }
        state
    }
}

impl From<std::io::Error> for TrieError {
    fn from(err: std::io::Error) -> Self {
        Self::Corrupt(format!("io: {err}"))
    }
}

impl From<StoreError> for TrieError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Mdbx(err) => Self::Mdbx(err),
            other => Self::Corrupt(other.to_string()),
        }
    }
}

/// Reads `HashedAccounts`.
fn read_account<K>(txn: &Txn<K>, db: &Db, key: B256) -> Result<Option<Account>, TrieError> {
    match txn.get(db.dbi(Table::HashedAccounts), key.as_slice())? {
        Some(bytes) => Ok(Some(Account::from_bytes(bytes)?)),
        None => Ok(None),
    }
}

/// Reads one `HashedStorages` slot, `None` when not in the table.
fn read_slot<K>(txn: &Txn<K>, db: &Db, key: B256, slot: B256) -> Result<Option<U256>, TrieError> {
    let mut cursor = txn.cursor(db.dbi(Table::HashedStorages))?;
    match cursor.seek_dup(key.as_slice(), slot.as_slice())? {
        Some((_, value)) if value.starts_with(slot.as_slice()) => Ok(Some(StorageEntry::from_bytes(value)?.value)),
        _ => Ok(None),
    }
}

/// Every slot of one account's `HashedStorages`.
fn read_storage<K>(txn: &Txn<K>, db: &Db, key: B256) -> Result<Vec<(B256, U256)>, TrieError> {
    let mut cursor = txn.cursor(db.dbi(Table::HashedStorages))?;
    let mut out = Vec::new();
    let mut entry = cursor.seek_exact(key.as_slice())?.map(|(_, v)| v.to_vec());
    while let Some(value) = entry {
        let e = StorageEntry::from_bytes(&value)?;
        out.push((e.key, e.value));
        entry = cursor.next_dup()?.map(|(_, v)| v.to_vec());
    }
    Ok(out)
}

/// The `TigerWipedStorage` marker of a wipe of empty storage.
const EMPTY_WIPE: &[u8] = &[0];

fn wipe_key(block: u64, key: B256) -> Vec<u8> {
    [block.to_be_bytes().as_slice(), key.as_slice()].concat()
}

fn wiped_slot(value: &[u8]) -> Result<Option<(B256, U256)>, TrieError> {
    if value == EMPTY_WIPE {
        return Ok(None);
    }
    let mut reader = Reader::new(value);
    let slot = reader.b256()?;
    let rest = value.len() - 32;
    Ok(Some((slot, reader.compact_u256(rest)?)))
}

/// The state database over a datadir.
///
/// A datadir holds either the full state in reth's layout, or a partial state (see
/// [`crate::partial`]) for a node that started at a recent block from execution witnesses. In a
/// partial state, reads, roots and proofs that need state not held locally fail with an error
/// for which [`TrieError::is_unknown`] holds; they never answer zero or absent for it.
#[derive(Debug)]
pub struct StateDb {
    /// The datadir.
    pub datadir: Datadir,
    partial: bool,
}

impl StateDb {
    /// Opens (creating if needed) the datadir at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self, TrieError> {
        let datadir = Datadir::open(path)?;
        let partial = PartialState::root(&datadir.db.read()?, &datadir.db)?.is_some();
        Ok(Self { datadir, partial })
    }

    /// Opens the partial-state datadir at `path`, creating it when new: its state is the one
    /// with root `root` after block `block`, of which nothing is held yet. An existing partial
    /// datadir opens as it is. Fill it with [`Self::absorb_witness`] before each block.
    pub fn open_partial(path: &std::path::Path, block: u64, root: B256) -> Result<Self, TrieError> {
        let mut db = Self::open(path)?;
        if db.partial {
            return Ok(db);
        }
        if db.tip()?.is_some() {
            return Err(TrieError::Corrupt("the datadir holds a full state".into()));
        }
        let mut w = db.write()?;
        w.set_label(block);
        let dbi = w.db().dbi(Table::Metadata);
        w.txn().put(dbi, PARTIAL_ROOT_KEY, root.as_slice())?;
        w.set_checkpoints(block)?;
        // Empty changesets at the start block, so views at it work and history begins there.
        db.datadir.static_files.append_changeset(Segment::AccountChangeSets, block, &[])?;
        db.datadir.static_files.append_changeset(Segment::StorageChangeSets, block, &[])?;
        w.commit()?;
        db.partial = true;
        Ok(db)
    }

    /// Whether the datadir holds a partial state.
    pub const fn is_partial(&self) -> bool {
        self.partial
    }

    /// The partial state's tip root as `txn` sees it, `None` for a full state.
    fn partial_root<K>(&self, txn: &Txn<K>) -> Result<Option<B256>, TrieError> {
        if !self.partial {
            return Ok(None);
        }
        let root = PartialState::root(txn, &self.datadir.db)?;
        Ok(Some(root.ok_or_else(|| TrieError::Corrupt("partial state without a root".into()))?))
    }

    /// The account with hashed address `key` at the stored tip.
    fn tip_account<K>(&self, txn: &Txn<K>, key: B256) -> Result<Option<Account>, TrieError> {
        let db = &self.datadir.db;
        if let Some(account) = read_account(txn, db, key)? {
            return Ok(Some(account));
        }
        let Some(root) = self.partial_root(txn)? else { return Ok(None) };
        let leaf = SparseTrie::new(None, root).get(&DbNodes { txn, db }, key)?;
        Ok(leaf.map(|leaf| PartialState::decode_account_leaf(&leaf)).transpose()?.map(|(account, _)| account))
    }

    /// The storage root of the account `key` at the stored tip of a partial state.
    fn tip_storage_root<K>(&self, txn: &Txn<K>, root: B256, key: B256) -> Result<B256, TrieError> {
        let leaf = SparseTrie::new(None, root).get(&DbNodes { txn, db: &self.datadir.db }, key)?;
        Ok(leaf.map(|leaf| PartialState::decode_account_leaf(&leaf)).transpose()?.map_or(EMPTY_ROOT, |(_, storage_root)| storage_root))
    }

    /// The value of hashed `slot` of the account `key` at the stored tip.
    fn tip_slot<K>(&self, txn: &Txn<K>, key: B256, slot: B256) -> Result<U256, TrieError> {
        let db = &self.datadir.db;
        if let Some(value) = read_slot(txn, db, key, slot)? {
            return Ok(value);
        }
        let Some(root) = self.partial_root(txn)? else { return Ok(U256::ZERO) };
        let storage_root = self.tip_storage_root(txn, root, key)?;
        let leaf = SparseTrie::new(Some(key), storage_root).get(&DbNodes { txn, db }, slot)?;
        match leaf {
            Some(leaf) => U256::decode(&mut &leaf[..]).map_err(|e| TrieError::Corrupt(format!("storage leaf: {e}"))),
            None => Ok(U256::ZERO),
        }
    }

    /// Stores what an execution witness for the next block reveals of the stored tip's state
    /// (partial states only): its trie nodes reachable from the tip root, the values of its
    /// leaves, and `codes`. Call it in the block's write transaction before
    /// [`Self::commit_block`]; nodes of other roots are ignored.
    pub fn absorb_witness(&self, w: &mut WriteTxn<'_>, nodes: &[Bytes], codes: &[Bytes]) -> Result<(), TrieError> {
        let db = &w.datadir.db;
        let root = self.partial_root(w.txn())?.ok_or_else(|| TrieError::Corrupt("witnesses fill only partial states".into()))?;
        PartialState::absorb_witness(w.txn(), db, root, nodes, codes)
    }

    /// The new state root after hashed `state`, writing the trie changes into `txn`.
    fn write_trie(&self, txn: &mut Txn<Rw>, state: &HashedState) -> Result<B256, TrieError> {
        let db = &self.datadir.db;
        if let Some(root) = self.partial_root(txn)? {
            let tries = StateTries::apply(&DbNodes { txn, db }, root, state)?;
            tries.write(txn, db)?;
            return Ok(tries.root);
        }
        let (root, updates) = TrieView::new(txn, db, state).state_root(true)?;
        StateWriter::write_trie_updates(txn, db, &updates)?;
        Ok(root)
    }

    /// The last committed block, `None` before genesis.
    pub fn tip(&self) -> Result<Option<u64>, TrieError> {
        Ok(self.datadir.tip()?)
    }

    /// Begins the write transaction (shared with the node's own tables).
    pub fn write(&self) -> Result<WriteTxn<'_>, TrieError> {
        Ok(self.datadir.write()?)
    }

    /// Writes block `number`'s state changes into `w`: flat state, trie nodes, code, changesets,
    /// history and every stage checkpoint. Blocks are committed in order from genesis (any block
    /// number, its allocations as changes). Returns the state root after the block.
    pub fn commit_block(&self, w: &mut WriteTxn<'_>, number: u64, changes: &BlockChanges) -> Result<B256, TrieError> {
        let datadir = w.datadir;
        let db = &datadir.db;
        // The first block (genesis) may have any number; later blocks follow the tip.
        if let Some(tip) = Datadir::tip_in(w.txn(), db)?
            && number != tip + 1
        {
            return Err(TrieError::Corrupt(format!("committing block {number}, expected {}", tip + 1)));
        }
        w.set_label(number);
        let n = changes.normalized();
        let txn = w.txn();
        // Prior values for the changesets.
        let mut account_rows = Vec::new();
        for (address, info) in &n.accounts {
            let prior = self.tip_account(txn, hash_key(address.as_slice()))?;
            if prior != info.map(AccountInfo::to_account) {
                account_rows.push(AccountBeforeTx { address: *address, info: prior });
            }
        }
        let mut storage_rows = Vec::new();
        for ((address, slot), value) in &n.storage {
            let prior = self.tip_slot(txn, hash_key(address.as_slice()), hash_key(slot.as_slice()))?;
            if prior != *value {
                storage_rows.push(StorageBeforeTx { address: *address, slot: *slot, value: prior });
            }
        }
        // Whole storages removed by wipes and deletions.
        let deleted = n.accounts.iter().filter(|(_, i)| i.is_none()).map(|(a, _)| *a);
        let wiped: BTreeSet<Address> = n.wiped.iter().copied().chain(deleted).collect();
        let mut wipes = Vec::new();
        for address in &wiped {
            let key = hash_key(address.as_slice());
            if let Some(root) = self.partial_root(txn)?
                && self.tip_storage_root(txn, root, key)? != EMPTY_ROOT
            {
                // A partial state cannot record what the wipe removes. Base wipes only empty
                // storage: EIP-6780 deletes only accounts created in the same transaction, and
                // EIP-7610 forbids creating over storage.
                return Err(TrieError::Unknown(format!("storage of {address} wiped by block {number}")));
            }
            let slots = read_storage(txn, db, key)?;
            if slots.is_empty() && self.tip_account(txn, key)?.is_none() {
                continue;
            }
            wipes.push((key, slots));
        }
        for (key, slots) in &wipes {
            txn.put(db.dbi(Table::TigerWipes), &[key.as_slice(), &number.to_be_bytes()].concat(), &[])?;
            let wkey = wipe_key(number, *key);
            if slots.is_empty() {
                txn.put(db.dbi(Table::TigerWipedStorage), &wkey, EMPTY_WIPE)?;
            }
            for (slot, value) in slots {
                let mut entry = slot.to_vec();
                put_compact_u256(&mut entry, *value);
                txn.put(db.dbi(Table::TigerWipedStorage), &wkey, &entry)?;
            }
        }
        // Trie and flat state.
        let state = changes.hashed();
        let root = self.write_trie(txn, &state)?;
        StateWriter::write_hashed_state(txn, db, &state)?;
        for (hash, code) in &changes.codes {
            txn.put(db.dbi(Table::Bytecodes), hash.as_slice(), &BytecodeCodec::encode(code))?;
        }
        w.set_checkpoints(number)?;
        // Changesets and history.
        let rows: Vec<Vec<u8>> = account_rows.iter().map(AccountBeforeTx::to_bytes).collect();
        datadir.static_files.append_changeset(Segment::AccountChangeSets, number, &rows)?;
        let rows: Vec<Vec<u8>> = storage_rows.iter().map(StorageBeforeTx::to_bytes).collect();
        datadir.static_files.append_changeset(Segment::StorageChangeSets, number, &rows)?;
        for row in &account_rows {
            w.add_history(RocksTable::AccountsHistory, row.address.to_vec(), number);
        }
        for row in &storage_rows {
            w.add_history(RocksTable::StoragesHistory, [row.address.as_slice(), row.slot.as_slice()].concat(), number);
        }
        Ok(root)
    }

    /// Writes hashed state changes as block `number` without changesets or history, for callers
    /// that have no address and slot preimages. Such a datadir cannot unwind natively or answer
    /// historical reads. Returns the state root.
    pub fn commit_hashed(&self, w: &mut WriteTxn<'_>, number: u64, state: &HashedState) -> Result<B256, TrieError> {
        let db = &w.datadir.db;
        w.set_label(number);
        let txn = w.txn();
        let root = self.write_trie(txn, state)?;
        StateWriter::write_hashed_state(txn, db, state)?;
        w.set_checkpoints(number)?;
        Ok(root)
    }

    /// The hashed state that turns the state after block `last` back into the state after block
    /// `first - 1`, from the changesets and wipe records of blocks `first..=last`.
    fn reverts<K>(&self, txn: &Txn<K>, first: u64, last: u64) -> Result<HashedState, TrieError> {
        let sf = &self.datadir.static_files;
        let mut state = HashedState::default();
        let mut block = last;
        while block >= first {
            let missing = || TrieError::Corrupt(format!("changesets of block {block} are not stored"));
            for row in sf.changeset(Segment::AccountChangeSets, block)?.ok_or_else(missing)? {
                let row = AccountBeforeTx::from_bytes(&row)?;
                let key = hash_key(row.address.as_slice());
                match row.info {
                    Some(account) => state.set_account(key, account),
                    None => state.delete_account(key),
                }
            }
            self.apply_wipes(txn, block, &mut state)?;
            for row in sf.changeset(Segment::StorageChangeSets, block)?.ok_or_else(missing)? {
                let row = StorageBeforeTx::from_bytes(&row)?;
                state.set_slot(hash_key(row.address.as_slice()), hash_key(row.slot.as_slice()), row.value);
            }
            if block == 0 {
                break;
            }
            block -= 1;
        }
        Ok(state)
    }

    /// Replaces the storage of every account wiped in `block` with what the wipe removed.
    fn apply_wipes<K>(&self, txn: &Txn<K>, block: u64, state: &mut HashedState) -> Result<(), TrieError> {
        let db = &self.datadir.db;
        let mut cursor = txn.cursor(db.dbi(Table::TigerWipedStorage))?;
        let prefix = block.to_be_bytes();
        let mut current: Option<(B256, HashedStorage)> = None;
        let mut entry = cursor.seek(&prefix)?.map(|(k, v)| (k.to_vec(), v.to_vec()));
        while let Some((key, value)) = entry {
            if !key.starts_with(&prefix) {
                break;
            }
            let account = B256::from_slice(&key[8..]);
            if current.as_ref().is_some_and(|(a, _)| *a != account) {
                let (a, s) = current.take().expect("current");
                state.storages.insert(a, s);
            }
            let storage = &mut current.get_or_insert_with(|| (account, HashedStorage { wiped: true, slots: BTreeMap::new() })).1;
            if let Some((slot, value)) = wiped_slot(&value)? {
                storage.slots.insert(slot, value);
            }
            entry = cursor.next()?.map(|(k, v)| (k.to_vec(), v.to_vec()));
        }
        if let Some((a, s)) = current {
            state.storages.insert(a, s);
        }
        Ok(())
    }

    /// Unwinds the stored chain to block `number` inside `w`, with the block data of the blocks
    /// above it (the node removes its own tables' rows in the same transaction). The static
    /// files, transaction hashes and history are cut after the commit, so commit `w` before
    /// committing new blocks: one transaction cannot both unwind and append.
    /// Returns the state root at `number`.
    pub fn unwind_in(&self, w: &mut WriteTxn<'_>, number: u64) -> Result<B256, TrieError> {
        let db = &w.datadir.db;
        let tip = Datadir::tip_in(w.txn(), db)?.ok_or_else(|| TrieError::Corrupt("unwind of an empty datadir".into()))?;
        if number > tip {
            return Err(TrieError::Corrupt(format!("unwind to {number} above the tip {tip}")));
        }
        w.set_label(number);
        let state = {
            let txn = w.txn();
            self.reverts(txn, number + 1, tip)?
        };
        let txn = w.txn();
        let root = self.write_trie(txn, &state)?;
        StateWriter::write_hashed_state(txn, db, &state)?;
        // Drop wipe records above `number`.
        let mut stale = Vec::new();
        {
            let mut cursor = txn.cursor(db.dbi(Table::TigerWipedStorage))?;
            let mut entry = cursor.seek(&(number + 1).to_be_bytes())?.map(|(k, _)| k.to_vec());
            while let Some(key) = entry {
                if stale.last() != Some(&key) {
                    stale.push(key);
                }
                entry = cursor.next_key()?.map(|(k, _)| k.to_vec());
            }
        }
        for key in &stale {
            txn.delete(db.dbi(Table::TigerWipedStorage), key, None)?;
            let wipes_key = [&key[8..], &key[..8]].concat();
            txn.delete(db.dbi(Table::TigerWipes), &wipes_key, None)?;
        }
        w.unwind_blocks(number, tip)?;
        w.set_checkpoints(number)?;
        w.heal_after_commit();
        Ok(root)
    }

    /// Unwinds to block `number` in its own transaction.
    pub fn unwind_to(&self, number: u64) -> Result<B256, TrieError> {
        let mut w = self.write()?;
        let root = self.unwind_in(&mut w, number)?;
        w.commit()?;
        Ok(root)
    }

    /// A read view of the state after stored block `block` with `overlays` (blocks after it,
    /// oldest first) applied in memory.
    pub fn view(&self, block: u64, overlays: Vec<BlockChanges>) -> Result<StateView<'_>, TrieError> {
        let txn = self.datadir.db.read()?;
        let tip = Datadir::tip_in(&txn, &self.datadir.db)?.ok_or_else(|| TrieError::Corrupt("empty datadir".into()))?;
        if block > tip {
            return Err(TrieError::Corrupt(format!("view at {block} above the tip {tip}")));
        }
        if let Some(genesis) = self.datadir.static_files.first_block(Segment::AccountChangeSets)
            && block < genesis
        {
            return Err(TrieError::Corrupt(format!("view at {block} below the first stored block {genesis}")));
        }
        let overlays = overlays.into_iter().map(|c| (c.normalized(), c)).collect();
        Ok(StateView { db: self, txn, block, tip, overlays, state: std::cell::OnceCell::new() })
    }
}

/// A consistent read view: stored block `block` plus in-memory blocks.
#[derive(Debug)]
pub struct StateView<'d> {
    db: &'d StateDb,
    txn: Txn<Ro>,
    block: u64,
    tip: u64,
    overlays: Vec<(Normalized, BlockChanges)>,
    /// Reverts to `block` plus the overlays, in hashed space, built on first use.
    state: std::cell::OnceCell<HashedState>,
}

impl StateView<'_> {
    fn dbs(&self) -> &Db {
        &self.db.datadir.db
    }

    /// The hashed overlay of this view over the current tables.
    fn hashed(&self) -> Result<&HashedState, TrieError> {
        if let Some(state) = self.state.get() {
            return Ok(state);
        }
        let mut state =
            if self.block < self.tip { self.db.reverts(&self.txn, self.block + 1, self.tip)? } else { HashedState::default() };
        for (_, changes) in &self.overlays {
            state.extend(&changes.hashed());
        }
        Ok(self.state.get_or_init(|| state))
    }

    /// The account at `address`.
    pub fn account(&self, address: Address) -> Result<Option<AccountInfo>, TrieError> {
        for (n, _) in self.overlays.iter().rev() {
            if let Some(info) = n.accounts.get(&address) {
                return Ok(*info);
            }
        }
        let key = hash_key(address.as_slice());
        if self.block < self.tip {
            let rocks = &self.db.datadir.rocks;
            let change = History::first_change_after(rocks, RocksTable::AccountsHistory, address.as_slice(), self.block)
                .map_err(TrieError::Corrupt)?;
            if let Some(change) = change.filter(|c| *c <= self.tip) {
                let rows = self.db.datadir.static_files.changeset(Segment::AccountChangeSets, change)?.unwrap_or_default();
                for row in rows {
                    let row = AccountBeforeTx::from_bytes(&row)?;
                    if row.address == address {
                        return Ok(row.info.map(AccountInfo::from_account));
                    }
                }
                return Err(TrieError::Corrupt(format!("history names block {change} for {address}, its changeset does not")));
            }
        }
        Ok(self.db.tip_account(&self.txn, key)?.map(AccountInfo::from_account))
    }

    /// The value of `slot` of `address` (zero when unset).
    pub fn storage(&self, address: Address, slot: U256) -> Result<U256, TrieError> {
        let slot = B256::from(slot.to_be_bytes::<32>());
        for (n, _) in self.overlays.iter().rev() {
            if let Some(value) = n.storage.get(&(address, slot)) {
                return Ok(*value);
            }
            if n.wiped.contains(&address) || n.accounts.get(&address).is_some_and(Option::is_none) {
                return Ok(U256::ZERO);
            }
        }
        let key = hash_key(address.as_slice());
        let hashed_slot = hash_key(slot.as_slice());
        if self.block < self.tip {
            let rocks = &self.db.datadir.rocks;
            let prefix = [address.as_slice(), slot.as_slice()].concat();
            let change = History::first_change_after(rocks, RocksTable::StoragesHistory, &prefix, self.block)
                .map_err(TrieError::Corrupt)?
                .filter(|c| *c <= self.tip);
            let wipe = self.first_wipe_after(key)?;
            match (change, wipe) {
                (_, Some(wipe)) if change.is_none_or(|c| wipe <= c) => return self.wiped_value(wipe, key, hashed_slot),
                (Some(change), _) => {
                    let rows = self.db.datadir.static_files.changeset(Segment::StorageChangeSets, change)?.unwrap_or_default();
                    for row in rows {
                        let row = StorageBeforeTx::from_bytes(&row)?;
                        if row.address == address && row.slot == slot {
                            return Ok(row.value);
                        }
                    }
                    return Err(TrieError::Corrupt(format!("history names block {change} for a slot of {address}")));
                }
                _ => {}
            }
        }
        self.db.tip_slot(&self.txn, key, hashed_slot)
    }

    /// The first block after this view's block that wiped `key`'s storage.
    fn first_wipe_after(&self, key: B256) -> Result<Option<u64>, TrieError> {
        let mut cursor = self.txn.cursor(self.dbs().dbi(Table::TigerWipes))?;
        let seek = [key.as_slice(), &(self.block + 1).to_be_bytes()].concat();
        match cursor.seek(&seek)? {
            Some((k, _)) if k.len() == 40 && k.starts_with(key.as_slice()) => {
                let block = u64::from_be_bytes(k[32..].try_into().expect("8"));
                Ok((block <= self.tip).then_some(block))
            }
            _ => Ok(None),
        }
    }

    fn wiped_value(&self, block: u64, key: B256, slot: B256) -> Result<U256, TrieError> {
        let mut cursor = self.txn.cursor(self.dbs().dbi(Table::TigerWipedStorage))?;
        match cursor.seek_dup(&wipe_key(block, key), slot.as_slice())? {
            Some((_, value)) if value.starts_with(slot.as_slice()) => Ok(wiped_slot(value)?.map_or(U256::ZERO, |(_, v)| v)),
            _ => Ok(U256::ZERO),
        }
    }

    /// The code with `hash`, if stored (in a partial state, code not held is unknown).
    pub fn code(&self, hash: B256) -> Result<Option<Bytes>, TrieError> {
        if hash == KECCAK_EMPTY {
            return Ok(Some(Bytes::new()));
        }
        for (_, changes) in self.overlays.iter().rev() {
            if let Some((_, code)) = changes.codes.iter().find(|(h, _)| *h == hash) {
                return Ok(Some(code.clone()));
            }
        }
        match self.txn.get(self.dbs().dbi(Table::Bytecodes), hash.as_slice())? {
            Some(bytes) => Ok(Some(BytecodeCodec::decode(bytes)?.into())),
            None if self.db.partial => Err(TrieError::Unknown(format!("code {hash}"))),
            None => Ok(None),
        }
    }

    /// The state root of this view, with `extra` applied on top when given (nothing is written).
    pub fn state_root(&self, extra: Option<&BlockChanges>) -> Result<B256, TrieError> {
        let base = self.hashed()?;
        let owned;
        let state = match extra {
            Some(extra) => {
                let mut state = base.clone();
                state.extend(&extra.hashed());
                owned = state;
                &owned
            }
            None => base,
        };
        if let Some(root) = self.db.partial_root(&self.txn)? {
            return Ok(StateTries::apply(&self.nodes(), root, state)?.root);
        }
        Ok(TrieView::new(&self.txn, self.dbs(), state).state_root(false)?.0)
    }

    fn nodes(&self) -> DbNodes<'_, Ro> {
        DbNodes { txn: &self.txn, db: self.dbs() }
    }

    /// The storage root of `key` after hashed `state` over the stored tables.
    fn storage_root_in(&self, state: &HashedState, key: B256) -> Result<B256, TrieError> {
        if let Some(root) = self.db.partial_root(&self.txn)? {
            return StateTries::apply(&self.nodes(), root, state)?.storage_root(&self.nodes(), key);
        }
        TrieView::new(&self.txn, self.dbs(), state).storage_root(key)
    }

    /// The trie nodes a root computation of `extra` over this view loads: the nodes on the path
    /// of every changed key, and the siblings that deletions collapse into. A partial state
    /// answers only at its stored tip without overlays.
    pub fn trie_witness(&self, extra: &BlockChanges) -> Result<Vec<Bytes>, TrieError> {
        let changes = extra.hashed();
        if self.db.partial {
            if self.block != self.tip || !self.overlays.is_empty() {
                return Err(TrieError::Unknown("witness below the tip of a partial state".into()));
            }
            let root = self.db.partial_root(&self.txn)?.expect("partial");
            let nodes = self.nodes();
            let recording = Recording { inner: &nodes, seen: Default::default() };
            StateTries::apply(&recording, root, &changes)?;
            return Ok(recording.seen.into_inner().into_values().collect());
        }
        let base = self.hashed()?;
        let view = TrieView::new(&self.txn, self.dbs(), base);
        let root = view.state_root(false)?.0;
        let source = ProofNodes { view: &view };
        let recording = Recording { inner: &source, seen: Default::default() };
        StateTries::apply(&recording, root, &changes)?;
        Ok(recording.seen.into_inner().into_values().collect())
    }

    /// The storage root of `address` with `extra` applied on top (nothing is written).
    pub fn storage_root_after(&self, address: Address, extra: &BlockChanges) -> Result<B256, TrieError> {
        let mut state = self.hashed()?.clone();
        state.extend(&extra.hashed());
        self.storage_root_in(&state, hash_key(address.as_slice()))
    }

    /// The storage root of the account with hashed address `key`.
    pub fn storage_root_hashed(&self, key: B256) -> Result<B256, TrieError> {
        self.storage_root_in(self.hashed()?, key)
    }

    /// The storage root of `address`.
    pub fn storage_root(&self, address: Address) -> Result<B256, TrieError> {
        self.storage_root_hashed(hash_key(address.as_slice()))
    }

    /// The proof of the account with hashed address `key` and its hashed `slots`.
    pub fn proof_hashed(&self, key: B256, slots: &[B256]) -> Result<AccountProof, TrieError> {
        if let Some(root) = self.db.partial_root(&self.txn)? {
            return StateTries::apply(&self.nodes(), root, self.hashed()?)?.proof(&self.nodes(), key, slots);
        }
        TrieView::new(&self.txn, self.dbs(), self.hashed()?).proof(key, slots)
    }

    /// The `eth_getProof` proof of `address` and `slots`.
    pub fn proof(&self, address: Address, slots: &[U256]) -> Result<AccountProof, TrieError> {
        let slots: Vec<B256> = slots.iter().map(|s| hash_key(&s.to_be_bytes::<32>())).collect();
        self.proof_hashed(hash_key(address.as_slice()), &slots)
    }

    /// The account with hashed address `key` in this view (no history: current tables plus the
    /// view's overlay).
    pub fn account_hashed(&self, key: B256) -> Result<Option<Account>, TrieError> {
        if let Some(account) = self.hashed()?.accounts.get(&key) {
            return Ok(*account);
        }
        self.db.tip_account(&self.txn, key)
    }
}
