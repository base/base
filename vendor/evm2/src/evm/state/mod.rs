//! Basic in-memory EVM host state.

mod account;
mod block;
mod journal;
mod pending;
mod storage;
mod stream;
mod tracked;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    mem,
    ops::{Deref, DerefMut},
};

pub(crate) use account::Account;
pub use account::{AccountHandle, AccountInfo};
use alloy_primitives::{
    Address, B256, KECCAK256_EMPTY, Log,
    map::{AddressMap, AddressSet, hash_map},
};
pub use block::BlockStateAccumulator;
use derive_where::derive_where;
pub use journal::{JournalEntry, StateCheckpoint};
pub use pending::PendingState;
pub use storage::{StorageHandle, StorageOverlay, StorageSlot, StorageSlotHandle};
pub use stream::{
    AccountChangeRef, NoopChangeSink, StateChangeSink, StateChangeSource, StorageChange, Tee,
};
pub use tracked::Tracked;

use super::{
    PrewarmSet,
    bal::{Bal, BalError, BlockAccessIndex},
    db::{CacheDB, DbResult, DynDatabase, boxed_dyn_database},
};
use crate::{
    ErrorCode, EvmFeatures, Version,
    bytecode::Bytecode,
    interpreter::{InstrStop, Word},
    storage_key::{StorageKey, StorageKeyMap},
};

/// Mutable EVM state with an accepted-state cache, transaction layer, and reversible journal.
#[derive(Debug)]
#[non_exhaustive]
pub struct State<'a> {
    /// Account writes plus touch and warm-access metadata for the current transaction.
    accounts: AddressMap<Account>,
    /// Persistent storage writes plus warm slot metadata for the current transaction.
    storage: AddressMap<StorageOverlay>,
    /// Transaction-scoped EIP-1153 transient storage keyed by account address and slot.
    transient_storage: StorageKeyMap<Word>,
    /// Inner state.
    inner: StateInner<'a>,
}

impl<'a> Deref for State<'a> {
    type Target = StateInner<'a>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a> DerefMut for State<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Shared inner state borrowed by journaled mutation handles.
///
/// Holds the parts of [`State`] that a [`AccountHandle`] or [`StorageHandle`] needs while it
/// borrows an account or storage overlay: the backing database, the revert journal, and the
/// pre-warmed set. Splitting these out of [`State`] lets a handle borrow them
/// together as one `&mut StateInner` disjointly from the account/storage maps it mutates. [`State`]
/// derefs to this type, so its fields and methods are reachable directly on a [`State`].
#[derive_where(Debug)]
#[non_exhaustive]
pub struct StateInner<'a> {
    /// Database plus accepted transaction-boundary state overlay.
    #[derive_where(skip)]
    database: CacheDB<Box<dyn DynDatabase + 'a>>,
    /// Pre-warmed set: precompiles, coinbase, and the EIP-2930 access list.
    prewarm_set: PrewarmSet,
    /// Revert journal.
    journal: Vec<JournalEntry>,
    /// Logs emitted by the current transaction.
    logs: Vec<Log>,
    /// Accounts self-destructed in the current transaction.
    selfdestructs: AddressSet,
}

impl<'a> State<'a> {
    /// Creates a new state over an initial database.
    pub fn new(initial: impl DynDatabase + 'a) -> Self {
        Self::new_mono(boxed_dyn_database(initial))
    }

    pub(crate) fn new_mono(initial: Box<dyn DynDatabase + 'a>) -> Self {
        Self {
            accounts: AddressMap::default(),
            storage: AddressMap::default(),
            transient_storage: StorageKeyMap::default(),
            inner: StateInner {
                database: CacheDB::new(initial),
                prewarm_set: PrewarmSet::new(),
                journal: Vec::new(),
                logs: Vec::new(),
                selfdestructs: AddressSet::default(),
            },
        }
    }

    /// Returns a checkpoint for later rollback.
    #[inline]
    pub const fn checkpoint(&self) -> StateCheckpoint {
        StateCheckpoint::new(self.inner.journal.len(), self.inner.logs.len())
    }

    /// Returns the initial database.
    #[inline]
    pub fn initial(&self) -> &(dyn DynDatabase + 'a) {
        self.database.db.as_ref()
    }

    /// Returns the initial database mutably.
    #[inline]
    pub fn initial_mut(&mut self) -> &mut (dyn DynDatabase + 'a) {
        self.database.db.as_mut()
    }

    /// Replaces the initial database and clears all in-memory state layers.
    #[inline]
    pub fn set_initial(&mut self, initial: impl DynDatabase + 'a) {
        self.database = CacheDB::new(boxed_dyn_database(initial));
        self.clear_transaction_state();
    }

    /// Returns the accepted-state overlay database.
    #[inline]
    pub fn overlay_db(&self) -> &CacheDB<Box<dyn DynDatabase + 'a>> {
        &self.inner.database
    }

    /// Returns the accepted-state overlay database mutably.
    #[inline]
    pub fn overlay_db_mut(&mut self) -> &mut CacheDB<Box<dyn DynDatabase + 'a>> {
        &mut self.inner.database
    }

    /// Applies borrowed changes to the accepted state overlay.
    #[inline]
    pub fn commit_source<S: StateChangeSource>(&mut self, source: &S) {
        self.inner.database.commit_source(source);
    }

    /// Attaches an EIP-7928 BAL that the accepted-overlay database consults on reads.
    ///
    /// Once attached, account-info and storage reads are served from the BAL at the current block
    /// access index (layered over the cache/database). Reads not covered by the BAL error unless
    /// [`Self::set_allow_bal_db_fallback`] is enabled.
    #[inline]
    pub fn set_bal(&mut self, bal: Arc<Bal>) {
        self.inner.database.bal_context.set_bal(bal);
    }

    /// Returns the attached read BAL, or `None` when no BAL is attached.
    #[inline]
    pub const fn bal(&self) -> Option<&Arc<Bal>> {
        self.inner.database.bal_context.bal()
    }

    /// Detaches the read BAL, so reads resolve from the cache/database again.
    #[inline]
    pub fn clear_bal(&mut self) {
        self.inner.database.bal_context.clear_bal();
    }

    /// Takes the stashed BAL lookup error, if a read left one.
    ///
    /// Lets a caller report which address or slot a refused read wanted, which the
    /// [`ErrorCode::BAL_NOT_COVERED`](crate::ErrorCode::BAL_NOT_COVERED) sentinel
    /// on the returned error does not carry.
    #[inline]
    pub const fn take_bal_error(&mut self) -> Option<BalError> {
        self.inner.database.bal_context.take_bal_error()
    }

    /// Sets whether reads not covered by the attached BAL fall back to the cache/database instead
    /// of erroring.
    #[inline]
    pub const fn set_allow_bal_db_fallback(&mut self, allow: bool) {
        self.inner.database.bal_context.set_allow_db_fallback(allow);
    }

    /// Enables EIP-7928 Block Access List construction on the accepted-overlay database.
    ///
    /// Once enabled, every committed transaction is folded into the builder at the current block
    /// access index. Bump the index once per transaction with [`Self::bump_bal_index`]. The BAL
    /// state lives in the accepted-overlay [`CacheDB`]'s
    /// [`BalContext`](crate::evm::BalContext).
    #[inline]
    pub fn enable_bal_builder(&mut self) {
        self.inner.database.bal_context.enable_bal_builder();
    }

    /// Returns the in-progress BAL builder, or `None` when BAL construction is disabled.
    #[inline]
    pub const fn bal_builder(&self) -> Option<&Bal> {
        self.inner.database.bal_context.bal_builder()
    }

    /// Returns the current EIP-7928 block access index.
    #[inline]
    pub const fn bal_index(&self) -> BlockAccessIndex {
        self.inner.database.bal_context.bal_index()
    }

    /// Resets the BAL block access index to the pre-execution slot. Call before a block's
    /// transactions.
    #[inline]
    pub const fn reset_bal_index(&mut self) {
        self.inner.database.bal_context.reset_bal_index();
    }

    /// Sets the BAL block access index to the given value. Use to position reads at an arbitrary
    /// transaction index, e.g. when executing on top of a read BAL mid-block.
    #[inline]
    pub const fn set_bal_index(&mut self, index: BlockAccessIndex) {
        self.inner.database.bal_context.set_bal_index(index);
    }

    /// Bumps the BAL block access index by one. Call once per transaction.
    #[inline]
    pub const fn bump_bal_index(&mut self) {
        self.inner.database.bal_context.bump_bal_index();
    }

    /// Takes the built BAL, resetting the block access index. Returns `None` when BAL construction
    /// is disabled.
    #[inline]
    pub const fn take_bal_builder(&mut self) -> Option<Bal> {
        self.inner.database.bal_context.take_bal_builder()
    }

    /// Loads a historical block hash.
    #[inline]
    pub(crate) fn block_hash(&mut self, number: &Word) -> DbResult<B256> {
        self.database.get_block_hash(number)
    }

    /// Returns logs emitted by the current in-flight transaction.
    #[inline]
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// Returns the revert journal for the current in-flight transaction.
    #[inline]
    pub fn journal(&self) -> &[JournalEntry] {
        &self.inner.journal
    }

    /// Returns the current value of a storage slot from the transaction overlay, if it has been
    /// loaded or written this transaction.
    ///
    /// This is a non-loading `&self` peek of the transaction overlay only: it does not consult the
    /// accepted overlay or backing database, so it returns `None` for a slot not touched this
    /// transaction. Use [`Self::storage_slot_untracked`] to read through to the database.
    #[inline]
    pub fn get_storage(&self, address: &Address, key: &Word) -> Option<Word> {
        self.storage.get(address)?.slots.get(key).map(|slot| slot.value.current)
    }

    /// Reads a storage slot from the committed state (accepted overlay and backing database),
    /// ignoring the in-flight transaction overlay.
    #[inline]
    pub fn read_committed_storage(&mut self, address: &Address, key: &Word) -> DbResult<Word> {
        self.inner.database.get_storage(address, key)
    }

    /// Takes logs emitted by the current in-flight transaction.
    #[inline]
    pub(crate) fn take_logs(&mut self) -> Vec<Log> {
        mem::take(&mut self.inner.logs)
    }

    /// Records a transaction log.
    #[inline]
    pub fn log(&mut self, log: Log) {
        self.logs.push(log);
    }

    /// Returns the pre-warmed set (precompiles, coinbase, access list).
    ///
    /// This is not the complete EIP-2929 initial warm set -- sender and recipient are warmed per
    /// account instead. See [`PrewarmSet`].
    #[inline]
    #[must_use]
    pub const fn prewarm_set(&self) -> &PrewarmSet {
        &self.inner.prewarm_set
    }

    /// Returns the pre-warmed set mutably so callers can warm precompiles, the coinbase, the
    /// EIP-2930 access list, or non-revertible base warm accounts/slots.
    ///
    /// Entries added through this handle survive [`Self::rollback`] and are cleared per transaction
    /// by [`Self::clear_transaction_state`].
    #[inline]
    pub const fn prewarm_set_mut(&mut self) -> &mut PrewarmSet {
        &mut self.inner.prewarm_set
    }

    /// Marks an address as warm in the pre-warmed set. See [`PrewarmSet::warm`].
    #[inline]
    pub fn prewarm(&mut self, address: &Address) {
        self.inner.prewarm_set.warm(address);
    }

    /// Marks an address and a set of storage slots as warm in the pre-warmed set. See
    /// [`PrewarmSet::warm_storage`].
    #[inline]
    pub fn prewarm_storage(&mut self, address: &Address, slots: impl IntoIterator<Item = Word>) {
        self.inner.prewarm_set.warm_storage(address, slots);
    }

    /// Marks an address and a single storage slot as warm in the pre-warmed set.
    /// See [`PrewarmSet::warm_storage`].
    #[inline]
    pub fn prewarm_storage_slot(&mut self, address: &Address, key: Word) {
        self.prewarm_storage(address, [key]);
    }

    /// Replaces the pre-warmed set wholesale.
    ///
    /// Use [`PrewarmSet`]'s warming methods to populate the set. The installed set survives
    /// [`Self::rollback`] and is cleared per transaction by [`Self::clear_transaction_state`].
    #[inline]
    pub fn set_prewarm_set(&mut self, prewarm_set: PrewarmSet) {
        self.inner.prewarm_set = prewarm_set;
    }

    /// Clears transaction-scoped substate.
    pub fn clear_transaction_state(&mut self) {
        let Self {
            accounts,
            storage,
            transient_storage,
            inner: StateInner { prewarm_set, journal, selfdestructs, logs, database: _ },
        } = self;
        accounts.clear();
        storage.clear();
        transient_storage.clear();
        prewarm_set.clear();
        journal.clear();
        selfdestructs.clear();
        logs.clear();
    }

    /// Ensures the account is present in the transaction overlay, loading it from the backing
    /// database when it has not been loaded yet.
    ///
    /// When `skip_cold` is true and the account is not already in the overlay, the cold database
    /// read is skipped and [`ErrorCode::COLD_LOAD_SKIPPED`] is returned, leaving the overlay
    /// untouched. This mirrors revm's `skip_cold_load`/`ColdLoadSkipped` so callers can detect a
    /// cold access without paying for the load. With `skip_cold` false (or when the account is
    /// already in the overlay) the entry is always returned.
    ///
    /// A map entry exists only because it was loaded, so an occupied entry is returned as-is.
    ///
    /// The load itself is not journaled: the loaded entry holds `original == present` and is a
    /// harmless read cache that [`Self::rollback`] leaves in place. Only later warmth and value
    /// changes are journaled and reverted.
    #[inline(always)]
    fn account_raw<'h>(
        inner: &mut StateInner<'a>,
        accounts: &'h mut AddressMap<Account>,
        address: &Address,
        skip_cold: bool,
    ) -> DbResult<&'h mut Account> {
        match accounts.entry(*address) {
            hash_map::Entry::Occupied(entry) => {
                // An already-loaded account has no cold database read to skip, so the skip only
                // signals an unaffordable *cold* access. Runtime warmth (`is_warm`, seeded from
                // the prewarm set on load) decides coldness: an account warmed earlier this
                // execution is a cheap warm access and must not be forced out of gas.
                let account = entry.into_mut();
                if skip_cold && !account.is_warm && !inner.prewarm_set.is_warm(address) {
                    return Err(ErrorCode::COLD_LOAD_SKIPPED);
                }
                Ok(account)
            }
            hash_map::Entry::Vacant(entry) => {
                let is_warm = inner.prewarm_set.is_warm(address);
                if skip_cold && !is_warm {
                    return Err(ErrorCode::COLD_LOAD_SKIPPED);
                }
                let original = inner.database.get_account(address)?;
                let present = original.clone();
                Ok(entry.insert(Account { original, present, is_warm, ..Account::default() }))
            }
        }
    }

    /// Loads `address` into the transaction overlay and returns a journaled mutation handle.
    ///
    /// Unlike [`Self::account_info_untracked`], which reads the backing database without caching,
    /// this reads the account once and preserves it in the transaction overlay. The returned
    /// [`AccountHandle`] records a revert snapshot on its first mutation, so any changes made
    /// through it are undone together by [`Self::rollback`]. The account is materialized as empty
    /// only when it is first mutated while absent. This mirrors revm's `AccountHandle`.
    ///
    /// When `skip_cold_load` is true and the account has not been loaded into the overlay yet, the
    /// cold database read is skipped and [`ErrorCode::COLD_LOAD_SKIPPED`] is returned, leaving
    /// the overlay untouched. Callers that cannot afford a cold access use this to detect it
    /// without paying for the load. An already-loaded account always yields a handle.
    pub fn account(
        &mut self,
        address: &Address,
        skip_cold_load: bool,
    ) -> DbResult<AccountHandle<'_, 'a>> {
        Self::account_raw(&mut self.inner, &mut self.accounts, address, skip_cold_load)
            .map(|tracked| AccountHandle::new(*address, tracked, &mut self.inner))
    }

    /// Returns a journaled mutation handle to `address`'s persistent storage overlay.
    ///
    /// The returned [`StorageHandle`] ties the account's storage slots to the revert journal, so
    /// any slot warmed or written through it is undone together by [`Self::rollback`]. Slot values
    /// are read from the backing database lazily, only when a slot is loaded or first written. This
    /// mirrors [`Self::account`] on the storage side.
    ///
    /// This does not load or touch the owning account; callers that need the account materialized
    /// must do so separately via [`Self::account`].
    pub fn storage(&mut self, address: &Address) -> StorageHandle<'_, 'a> {
        let storage = self.storage.entry(*address).or_default();
        StorageHandle::new(*address, storage, &mut self.inner)
    }

    /// Returns a journaled mutation handle to a single persistent storage slot of `address`.
    ///
    /// This is [`Self::storage`] narrowed to one slot — a convenience for callers that need
    /// exactly one [`StorageSlotHandle`]. The slot is loaded on access (reading the backing
    /// database on first touch) and never skips a cold load, so this returns a [`DbResult`]. See
    /// [`StorageHandle::into_slot`] for the per-slot semantics, including cold-load skipping.
    pub fn storage_slot(
        &mut self,
        address: &Address,
        key: Word,
        skip_cold_load: bool,
    ) -> DbResult<StorageSlotHandle<'_, 'a>> {
        self.storage(address).into_slot(key, skip_cold_load)
    }

    /// Returns account info from the overlay or the backing database.
    ///
    /// This is a non-loading peek: it returns the overlay account when one has been loaded this
    /// transaction, otherwise it reads the backing database directly without caching the result in
    /// the overlay. Use [`Self::account`] when the account should be loaded and preserved.
    #[inline(never)]
    pub fn account_info_untracked(&mut self, address: &Address) -> DbResult<Option<AccountInfo>> {
        if let Some(entry) = self.accounts.get(address) {
            return Ok(entry.present.clone());
        }
        self.database.get_account(address)
    }

    /// Returns a single persistent storage slot's value from the overlay or the backing database.
    ///
    /// This is the storage-side mirror of [`Self::account_info_untracked`]: a non-loading peek that
    /// returns the overlay slot value when one has been loaded or written this transaction,
    /// otherwise it reads the backing database directly without caching the result in the overlay.
    /// A slot of a wiped account that has not been rewritten reads as zero. Use
    /// [`Self::storage_slot`] when the slot should be loaded and preserved.
    #[inline(never)]
    pub fn storage_slot_untracked(&mut self, address: &Address, key: &Word) -> DbResult<Word> {
        if let Some(overlay) = self.storage.get(address) {
            if let Some(slot) = overlay.slots.get(key) {
                return Ok(slot.value.current);
            }
            if overlay.wiped {
                return Ok(Word::ZERO);
            }
        }
        self.database.get_storage(address, key)
    }

    /// Transfers value between accounts.
    pub fn transfer(&mut self, from: &Address, to: &Address, value: &Word) -> DbResult<bool> {
        if value.is_zero() {
            self.account(to, false)?.touch();
            return Ok(true);
        }

        if from == to {
            let mut account = self.account(from, false)?;
            if account.balance() < *value {
                return Ok(false);
            }
            account.touch();
            return Ok(true);
        }

        {
            let mut from_account = self.account(from, false)?;
            let Some(new_from_balance) = from_account.balance().checked_sub(*value) else {
                return Ok(false);
            };
            // `set_balance` touches the account, matching the touch the prior `transfer` performed.
            from_account.set_balance(new_from_balance);
            from_account.touch();
        }
        {
            let mut to_account = self.account(to, false)?;
            let new_to_balance = to_account.balance().saturating_add(*value);
            to_account.set_balance(new_to_balance);
            to_account.touch();
        }
        Ok(true)
    }

    /// Creates a contract account and transfers endowment from the caller.
    #[inline(never)]
    pub fn create_account(
        &mut self,
        caller: &Address,
        address: &Address,
        value: &Word,
        features: EvmFeatures,
    ) -> DbResult<Result<(), InstrStop>> {
        // TODO check order of operations, we could potentially simplify it and do a lot more with
        // only one hashmap lookup.
        if self
            .account(address, false)?
            .get()
            .is_some_and(|account| account.nonce != 0 || account.code_hash != KECCAK256_EMPTY)
        {
            return Ok(Err(InstrStop::CreateCollision));
        }

        // Deduct the endowment from the caller. A zero endowment moves nothing and leaves the
        // caller untouched, matching the prior `transfer` behaviour.
        if !value.is_zero() {
            let mut caller_account = self.account(caller, false)?;
            let Some(new_caller_balance) = caller_account.balance().checked_sub(*value) else {
                return Ok(Err(InstrStop::OutOfFunds));
            };
            caller_account.set_balance(new_caller_balance);
        }

        let mut target = self.account(address, false)?;
        // Preserve any balance the address already held (e.g. funds sent before creation) and add
        // the endowment.
        let balance = target.balance().wrapping_add(*value);
        *target.get_or_insert() = AccountInfo {
            nonce: u64::from(features.contains(EvmFeatures::EIP161)),
            balance,
            code_hash: KECCAK256_EMPTY,
            code: Some(Bytecode::default()),
            _non_exhaustive: (),
        };
        target.mark_created();
        target.touch();
        Ok(Ok(()))
    }

    /// Loads transient (EIP-1153) storage.
    #[must_use]
    pub fn tload(&mut self, address: &Address, key: &Word) -> Word {
        self.transient_storage.get(&StorageKey::new(*address, *key)).copied().unwrap_or_default()
    }

    /// Stores transient (EIP-1153) storage.
    pub fn tstore(&mut self, address: &Address, key: &Word, value: &Word) {
        match self.transient_storage.entry(StorageKey::new(*address, *key)) {
            hash_map::Entry::Occupied(mut entry) => {
                let previous = *entry.get();
                if previous == *value {
                    return;
                }
                self.inner.journal.push(JournalEntry::TransientStorageChange {
                    address: *address,
                    key: *key,
                    previous: Some(previous),
                });
                if value.is_zero() {
                    entry.remove();
                } else {
                    *entry.get_mut() = *value;
                }
            }
            hash_map::Entry::Vacant(entry) => {
                if value.is_zero() {
                    return;
                }
                self.inner.journal.push(JournalEntry::TransientStorageChange {
                    address: *address,
                    key: *key,
                    previous: None,
                });
                entry.insert(*value);
            }
        }
    }

    /// Removes and returns every transient storage slot owned by `address`.
    ///
    /// This is intended for transaction-finalization policies that encode per-account pending
    /// work in EIP-1153 storage and must consume it before transaction scratch is cleared.
    pub fn take_transient_storage(&mut self, address: &Address) -> Vec<(Word, Word)> {
        let keys = self
            .transient_storage
            .keys()
            .copied()
            .filter(|key| key.address() == *address)
            .collect::<Vec<_>>();
        keys.into_iter()
            .map(|key| {
                let value = self
                    .transient_storage
                    .remove(&key)
                    .expect("collected transient storage key must exist");
                (key.key(), value)
            })
            .collect()
    }

    /// Reverts state changes after the checkpoint.
    #[inline(never)]
    pub fn rollback(&mut self, checkpoint: StateCheckpoint, features: EvmFeatures) {
        assert!(checkpoint.journal_len <= self.journal.len(), "checkpoint is past journal length");
        assert!(checkpoint.logs_len <= self.logs.len(), "checkpoint is past logs length");
        self.logs.truncate(checkpoint.logs_len);
        while self.journal.len() != checkpoint.journal_len {
            let Some(entry) = self.journal.pop() else {
                unreachable!("checkpoint is checked above")
            };
            match entry {
                JournalEntry::AccountChange {
                    address,
                    previous,
                    previous_is_warm,
                    previous_is_touched,
                    previous_is_destroyed,
                    previous_just_created,
                    previous_code_changed,
                } => {
                    // Reconcile the self-destruct set with the restored destroyed flag.
                    let was_destroyed =
                        self.accounts.get(&address).is_some_and(|entry| entry.is_destroyed);
                    if was_destroyed && !previous_is_destroyed {
                        self.selfdestructs.remove(&address);
                    } else if !was_destroyed && previous_is_destroyed {
                        self.selfdestructs.insert(address);
                    }
                    if let Some(entry) = self.accounts.get_mut(&address) {
                        entry.present = previous;
                        entry.is_warm = previous_is_warm;
                        // EIP-161 preserves the historical Yellow Paper K.1 precompile-3 touch.
                        if !(features.contains(EvmFeatures::EIP161)
                            && address == Address::with_last_byte(3))
                        {
                            entry.is_touched = previous_is_touched;
                        }
                        entry.is_destroyed = previous_is_destroyed;
                        entry.just_created = previous_just_created;
                        entry.code_changed = previous_code_changed;
                    }
                }
                JournalEntry::StorageChange { address, key, previous } => {
                    if let Some(storage) = self.storage.get_mut(&address)
                        && let Some(slot) = storage.slots.get_mut(&key)
                    {
                        slot.value.current = previous;
                    }
                }
                JournalEntry::TransientStorageChange { address, key, previous } => match previous {
                    Some(previous) if !previous.is_zero() => {
                        self.transient_storage.insert(StorageKey::new(address, key), previous);
                    }
                    _ => {
                        self.transient_storage.remove(&StorageKey::new(address, key));
                    }
                },
                JournalEntry::StorageWarmed { address, key } => {
                    if let Some(storage) = self.storage.get_mut(&address)
                        && let Some(slot) = storage.slots.get_mut(&key)
                    {
                        slot.is_warm = false;
                    }
                }
            }
        }
    }

    fn materialize_empty_account_for_finalization(&mut self, address: &Address) -> DbResult<()> {
        // `account_raw` loads the backing-database account into `original`,
        // so its existence is read from the same source rather than via a separate database read.
        let entry = Self::account_raw(&mut self.inner, &mut self.accounts, address, false)?;
        if entry.original.is_none() && entry.present.is_none() {
            // Finalization runs after the last revertible scope, so this is not journaled: the
            // entry would never be replayed before `clear_transaction_state` clears it.
            entry.present = Some(AccountInfo::default());
            entry.mark_created();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn finalize_transaction_(&mut self, version: &Version) {
        self.finalize_transaction(version).unwrap();
    }

    /// Applies transaction-finalization account-lifetime rules to the overlay.
    ///
    /// This mutates the in-memory post-transaction state before it is streamed to a
    /// [`StateChangeSink`] or detached as a [`PendingState`]. Runtime records
    /// transaction substate such as touches and selfdestructs, while finalization
    /// turns that substate into account deletions, storage wipes, balance-only
    /// selfdestruct resets (EIP-8246), or pre-EIP-161 empty-account materialization.
    pub(crate) fn finalize_transaction(&mut self, version: &Version) -> DbResult<()> {
        let selfdestructs = mem::take(&mut self.selfdestructs);
        let touched: Vec<_> = self
            .accounts
            .iter()
            .filter_map(|(&address, entry)| entry.is_touched.then_some(address))
            .collect();

        let eip8246 = version.feature(EvmFeatures::EIP8246);
        for address in &selfdestructs {
            // EIP-8246: a self-destructed account that still holds balance is preserved as a
            // balance-only account instead of being burned. One with no balance is removed. The
            // handle is scoped so its `AccountChange` flushes on drop before the storage wipe.
            {
                let mut account = self.account(address, false)?;
                if eip8246 && !account.balance().is_zero() {
                    account.reset_selfdestructed_for_finalization();
                } else {
                    account.delete_for_finalization();
                }
            }
            self.storage(address).wipe();
        }

        if version.feature(EvmFeatures::EIP161) {
            for address in &touched {
                // EIP-161 deletes touched dead accounts at transaction finalization.
                let mut account = self.account(address, false)?;
                if account.is_existing_dead() {
                    account.delete_for_finalization();
                    drop(account);
                    self.storage(address).wipe();
                }
            }
        } else {
            for address in &touched {
                // Before EIP-161, touching a non-existent account materializes it as empty.
                if !selfdestructs.contains(address) && !self.account(address, false)?.exists() {
                    self.materialize_empty_account_for_finalization(address)?;
                }
            }
        }

        // Restore the selfdestruct set without clearing it: `take_pending_state` moves it into the
        // detached [`PendingState`] to flag selfdestructed accounts, and `clear_transaction_state`
        // clears it at the end of the transaction lifecycle.
        self.selfdestructs = selfdestructs;

        for address in touched {
            if let Some(entry) = self.accounts.get_mut(&address) {
                entry.is_touched = false;
            }
        }
        Ok(())
    }

    /// Visits transaction state changes in database application order.
    ///
    /// This borrows changes directly from the transaction layer without detaching it and does not
    /// mutate the accepted overlay.
    pub(crate) fn visit_transaction_changes<S: StateChangeSink>(
        &self,
        sink: &mut S,
    ) -> Result<(), S::Error> {
        for entry in self.accounts.values() {
            if let Some((code_hash, code)) = entry.changed_code() {
                sink.bytecode(code_hash, code)?;
            }
        }

        for (&address, storage) in &self.storage {
            if storage.wiped {
                sink.storage_wipe(address)?;
            }
            for (&key, slot) in storage.changed_slots() {
                sink.storage(StorageChange {
                    address,
                    key,
                    original: slot.original,
                    current: slot.current,
                })?;
            }
        }

        for (&address, entry) in self.accounts.iter() {
            if entry.is_changed() {
                sink.account(AccountChangeRef {
                    address,
                    original: entry.original.as_ref(),
                    current: entry.present.as_ref(),
                    created: entry.is_created(),
                    selfdestructed: self.selfdestructs.contains(&address),
                })?;
            }
        }

        Ok(())
    }

    /// Detaches the transaction overlay into an owned [`PendingState`].
    ///
    /// The remaining transaction scratch (journal, logs, warm sets, transient storage) is left for
    /// [`Self::clear_transaction_state`].
    pub(crate) fn take_pending_state(&mut self) -> PendingState {
        PendingState {
            accounts: mem::take(&mut self.accounts),
            storage: mem::take(&mut self.storage),
            selfdestructs: mem::take(&mut self.inner.selfdestructs),
        }
    }

    /// Reattaches a detached [`PendingState`] as the current transaction overlay, replacing it.
    ///
    /// This is the inverse of the detach performed by
    /// [`ExecutedTx::detach`](crate::ExecutedTx::detach): the pending accounts, storage, and
    /// selfdestruct set become the transaction layer again, as if the transaction had just been
    /// finalized. Other transaction scratch (journal, logs, warm sets, transient storage) is not
    /// affected.
    pub fn set_pending_state(&mut self, pending: PendingState) {
        let PendingState { accounts, storage, selfdestructs } = pending;
        self.accounts = accounts;
        self.storage = storage;
        self.inner.selfdestructs = selfdestructs;
    }

    /// Accepts the current transaction's state transition into the accepted overlay.
    ///
    /// This advances the in-memory accepted overlay by the transaction's write-set and clears the
    /// transaction account/storage layers and selfdestruct markers. It does not take logs or write
    /// to the wrapped backing database.
    pub fn commit_transaction(&mut self) {
        // The transaction overlay is folded into the accepted-overlay database directly, without
        // detaching it.
        self.inner.database.commit(&self.accounts, &self.storage);
        self.accounts.clear();
        self.storage.clear();
        self.inner.selfdestructs.clear();
    }
}
