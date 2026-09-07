//! Transaction-scoped persistent storage overlay.

use alloy_primitives::{
    Address,
    map::{U256Map, hash_map},
};
use derive_where::derive_where;

use super::{DbResult, DynDatabase, JournalEntry, StateInner, Tracked};
use crate::{ErrorCode, interpreter::Word};

/// Persistent storage overlay for one account.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StorageOverlay {
    /// Whether consumers must delete all pre-existing storage for the account
    /// before applying individual slot changes.
    pub wiped: bool,
    /// Loaded storage slots. A slot is present here only once it has been loaded or written, so
    /// its value is always meaningful; EIP-2929 warmth is tracked per slot in
    /// [`StorageSlot::is_warm`].
    pub slots: U256Map<StorageSlot>,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl StorageOverlay {
    /// Returns the changed storage slots.
    ///
    /// A slot is changed when its current value differs from its transaction-boundary original,
    /// except slots of a wiped overlay whose current value is zero: the wipe already deletes them.
    #[inline]
    pub fn changed_slots(&self) -> impl Iterator<Item = (&Word, &Tracked<Word>)> {
        self.slots.iter().filter_map(|(key, slot)| {
            (slot.value.is_changed() && (!self.wiped || !slot.value.current.is_zero()))
                .then_some((key, &slot.value))
        })
    }
}

/// Persistent storage slot cached by [`super::State`].
///
/// A slot is held in the overlay only once it has been loaded or written, so its value is always
/// present. [`Self::is_warm`] records runtime EIP-2929 warmth observed during execution;
/// base/access-list warmth is held in the prewarm set, not here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageSlot {
    /// Tracked slot value: its transaction-boundary original together with the current value.
    pub value: Tracked<Word>,
    /// Whether the slot was warmed during execution this transaction, for EIP-2929 gas accounting.
    pub is_warm: bool,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl StorageSlot {
    /// Creates a freshly loaded slot whose original and current values are `value`, with the given
    /// EIP-2929 warmth.
    #[inline]
    fn loaded(value: Word, is_warm: bool) -> Self {
        Self { value: Tracked::new(value), is_warm, _non_exhaustive: () }
    }
}

/// A mutable, journaled handle to one account's persistent storage overlay.
///
/// Returned by [`State::storage`](super::State::storage). It ties the account's
/// [`StorageOverlay`] to the revert journal, the backing database, and the transaction-initial base
/// warm set, mirroring [`AccountHandle`](super::AccountHandle) on the storage side: a slot
/// mutation and its rollback bookkeeping cannot drift apart.
///
/// An individual slot is reached through [`Self::into_slot`], which loads the slot and yields a
/// [`StorageSlotHandle`] scoped to one key. Warmth without loading is answered cheaply by
/// [`Self::is_warm`] / [`Self::is_loaded`], so callers can make EIP-2929 cold/warm decisions before
/// paying for a cold database read.
#[derive_where(Debug)]
pub struct StorageHandle<'a, 'db> {
    /// Address of the account whose storage this handle exposes.
    address: Address,
    /// Transaction overlay entry: the per-account storage slots plus the wipe flag.
    storage: &'a mut StorageOverlay,
    /// Shared inner state: backing database, revert journal, and base warm set.
    #[derive_where(skip)]
    inner: &'a mut StateInner<'db>,
}

impl<'a, 'db> StorageHandle<'a, 'db> {
    /// Creates a handle over an account's storage overlay and the shared inner state (backing
    /// database, revert journal, and transaction-initial base warm set).
    #[inline]
    pub(crate) const fn new(
        address: Address,
        storage: &'a mut StorageOverlay,
        inner: &'a mut StateInner<'db>,
    ) -> Self {
        Self { address, storage, inner }
    }

    /// Returns the account address.
    #[inline]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns whether the account's storage is marked for a full wipe before individual slot
    /// changes are applied.
    #[inline]
    pub const fn is_wiped(&self) -> bool {
        self.storage.wiped
    }

    /// Returns whether the slot at `key` has already been loaded into the overlay this transaction.
    ///
    /// This is a pure overlay membership check: it does not consult the backing database or load
    /// anything, so callers can use it to detect a cold slot before paying for a load.
    #[inline]
    pub fn is_loaded(&self, key: &Word) -> bool {
        self.storage.slots.contains_key(key)
    }

    /// Returns whether the slot at `key` is warm for EIP-2929 gas accounting, consulting both the
    /// transaction's base warm set (EIP-2930 access-list slots) and runtime warmth recorded on an
    /// already-loaded slot.
    ///
    /// Like [`Self::is_loaded`], this neither loads the slot nor reads the backing database, so it
    /// can gate a cold access before the cold read is paid for.
    #[inline]
    pub fn is_warm(&self, key: &Word) -> bool {
        self.storage.slots.get(key).is_some_and(|slot| slot.is_warm)
            || self.inner.prewarm_set.is_storage_warm(&self.address, key)
    }

    /// Loads the slot at `key` into the overlay, reading the backing database on first access, and
    /// returns a journaled handle to it.
    ///
    /// When `skip_cold_load` is true and the slot is not already in the overlay, the cold database
    /// read is skipped and [`ErrorCode::COLD_LOAD_SKIPPED`] is returned, leaving the overlay
    /// untouched. This mirrors [`State::account`](super::State::account)'s
    /// `skip_cold_load`/`ColdLoadSkipped` so callers can detect a cold access without paying for
    /// the load. An already-loaded slot is always returned.
    ///
    /// A slot is materialized in the overlay only once it is loaded, so the returned
    /// [`StorageSlotHandle`] always refers to a slot with a meaningful value. On first load the
    /// slot's [`StorageSlot::is_warm`] is seeded from the base warm set (EIP-2930 access list),
    /// mirroring how [`account_raw`](super::State) seeds an account's warmth. The load itself
    /// records no revert entry: the cached value's original equals its current, so it is left in
    /// place by [`State::rollback`](super::State::rollback) as a harmless cache. Used by
    /// [`State::storage_slot`](super::State::storage_slot) to reach a single slot directly.
    #[inline]
    pub fn into_slot(
        self,
        key: Word,
        skip_cold_load: bool,
    ) -> DbResult<StorageSlotHandle<'a, 'db>> {
        let Self { address, storage, inner } = self;
        let slot = match storage.slots.entry(key) {
            hash_map::Entry::Occupied(entry) => {
                // An already-loaded slot has no cold database read to skip, so the skip only
                // signals an unaffordable *cold* access. Runtime warmth (`is_warm`, seeded from
                // the prewarm set on load) decides coldness: a slot warmed earlier this execution
                // is a cheap warm access and must not be forced out of gas.
                let slot = entry.into_mut();
                if skip_cold_load
                    && !slot.is_warm
                    && !inner.prewarm_set.is_storage_warm(&address, &key)
                {
                    return Err(ErrorCode::COLD_LOAD_SKIPPED);
                }
                slot
            }
            hash_map::Entry::Vacant(entry) => {
                let is_warm = inner.prewarm_set.is_storage_warm(&address, &key);
                if skip_cold_load && !is_warm {
                    return Err(ErrorCode::COLD_LOAD_SKIPPED);
                }
                let value = if storage.wiped {
                    Word::ZERO
                } else {
                    inner.database.get_storage(&address, &key)?
                };
                entry.insert(StorageSlot::loaded(value, is_warm))
            }
        };
        Ok(StorageSlotHandle { address, key, slot, inner })
    }

    /// Marks all of the account's prior persistent storage as deleted.
    ///
    /// Only called during transaction finalization (selfdestruct and EIP-161 dead-account
    /// deletion), after the last revertible scope, so the wipe is not journaled. Loaded slot
    /// entries are kept with their values reset to zero: wiped slots resolve to zero on re-load,
    /// and resetting `original` alongside `current` turns the transaction's prior writes into
    /// unchanged reads, which keeps a destroyed account's storage accesses visible to the EIP-7928
    /// block access list (execution-specs `destroy_storage` converts writes to reads).
    #[inline]
    pub fn wipe(&mut self) {
        self.storage.wiped = true;
        self.storage.slots.iter_mut().for_each(|(_, slot)| {
            slot.value.set_current(Word::ZERO);
        });
    }
}

/// A mutable, journaled handle to a single, loaded persistent storage slot.
///
/// Returned by [`StorageHandle::into_slot`]. Warming the slot records a
/// [`JournalEntry::StorageWarmed`] and writing it records a [`JournalEntry::StorageChange`], so
/// every effect made through the handle is undone together by
/// [`State::rollback`](super::State::rollback). A handle used only for reads records nothing.
///
/// The handle holds a mutable reference to the slot's overlay entry — which exists only because the
/// slot has been loaded, so its value is always meaningful — together with the shared
/// [`StateInner`] (backing database, revert journal, and base warm set) needed to journal effects
/// and answer warm-access queries.
#[derive_where(Debug)]
pub struct StorageSlotHandle<'a, 'db> {
    /// Address of the account that owns the slot.
    address: Address,
    /// Storage key of the slot.
    key: Word,
    /// The slot's loaded overlay entry: its tracked value and runtime warmth.
    slot: &'a mut StorageSlot,
    /// Shared inner state: backing database, revert journal, and base warm set.
    #[derive_where(skip)]
    inner: &'a mut StateInner<'db>,
}

impl StorageSlotHandle<'_, '_> {
    /// Returns the account address.
    #[inline]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns the storage key.
    #[inline]
    pub const fn key(&self) -> Word {
        self.key
    }

    /// Returns the tracked slot value.
    #[inline]
    pub const fn get(&self) -> &Tracked<Word> {
        &self.slot.value
    }

    /// Returns the current slot value.
    #[inline]
    pub const fn current(&self) -> Word {
        self.slot.value.current
    }

    /// Returns the slot's transaction-boundary original value.
    #[inline]
    pub const fn original(&self) -> Word {
        self.slot.value.original
    }

    /// Returns whether the slot is warm for EIP-2929 gas accounting, consulting both the
    /// transaction's base warm set (EIP-2930 access-list slots) and runtime warmth recorded during
    /// execution.
    #[inline]
    pub fn is_warm(&self) -> bool {
        self.slot.is_warm || self.inner.prewarm_set.is_storage_warm(&self.address, &self.key)
    }

    /// Marks the slot warm for EIP-2929 gas accounting, recording a [`JournalEntry::StorageWarmed`]
    /// when this access transitions it from cold to warm.
    ///
    /// Returns `true` if the slot was cold before this call. Slots already warm through the base
    /// warm set stay warm across rollback, so warming them again records nothing.
    #[inline]
    pub fn warm(&mut self) -> bool {
        if self.slot.is_warm {
            return false;
        }
        self.slot.is_warm = true;
        self.inner
            .journal
            .push(JournalEntry::StorageWarmed { address: self.address, key: self.key });
        true
    }

    /// Sets the slot value, recording a [`JournalEntry::StorageChange`] when the value actually
    /// changes. Writing the value the slot already holds records nothing.
    #[inline]
    pub fn set(&mut self, value: Word) {
        let previous = self.slot.value.current;
        if previous == value {
            return;
        }
        self.slot.value.current = value;
        self.inner.journal.push(JournalEntry::StorageChange {
            address: self.address,
            key: self.key,
            previous,
        });
    }

    /// Writes `value`, returning the slot's transaction-boundary original value and the value the
    /// slot held just before this write — the pair `SSTORE` net-gas metering needs.
    ///
    /// A revert entry is recorded only when the value actually changes, via [`Self::set`].
    #[inline]
    pub fn write(&mut self, value: Word) -> (Word, Word) {
        let original_value = self.slot.value.original;
        let present_value = self.slot.value.current;
        self.set(value);
        (original_value, present_value)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::Address;

    use super::*;
    use crate::{
        SpecId, Version,
        evm::{
            CacheDB,
            state::{AccountInfo, State},
        },
    };

    #[test]
    fn storage_change_rolls_back_to_checkpoint() {
        let address = Address::from([0x11; 20]);
        let mut database = CacheDB::default();
        database.insert_account_info(&address, AccountInfo::default());
        database.insert_account_storage(&address, &Word::from(1), &Word::from(10));
        let mut state = State::new(database);

        let checkpoint = state.checkpoint();
        state.storage_slot(&address, Word::from(1), false).unwrap().write(Word::from(20));
        state.storage_slot(&address, Word::from(1), false).unwrap().write(Word::from(30));

        assert_eq!(
            state.storage_slot(&address, Word::from(1), false).unwrap().current(),
            Word::from(30)
        );
        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert_eq!(
            state.storage_slot(&address, Word::from(1), false).unwrap().current(),
            Word::from(10)
        );
    }

    #[test]
    fn transient_storage_change_rolls_back_to_checkpoint() {
        let address = Address::from([0x22; 20]);
        let mut state = State::new(CacheDB::default());

        state.tstore(&address, &Word::from(1), &Word::from(10));
        let checkpoint = state.checkpoint();
        state.tstore(&address, &Word::from(1), &Word::from(20));

        assert_eq!(state.tload(&address, &Word::from(1)), Word::from(20));
        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert_eq!(state.tload(&address, &Word::from(1)), Word::from(10));
    }

    #[test]
    fn take_transient_storage_only_drains_requested_account() {
        let address = Address::from([0x22; 20]);
        let other = Address::from([0x23; 20]);
        let mut state = State::new(CacheDB::default());

        state.tstore(&address, &Word::from(1), &Word::from(10));
        state.tstore(&address, &Word::from(2), &Word::from(20));
        state.tstore(&other, &Word::from(1), &Word::from(30));

        let mut slots = state.take_transient_storage(&address);
        slots.sort_unstable_by_key(|(key, _)| *key);
        assert_eq!(slots, vec![(Word::from(1), Word::from(10)), (Word::from(2), Word::from(20))]);
        assert_eq!(state.tload(&address, &Word::from(1)), Word::ZERO);
        assert_eq!(state.tload(&other, &Word::from(1)), Word::from(30));
    }

    #[test]
    fn storage_wipe_preserves_warm_slots_in_merged_storage_map() {
        let account = Address::with_last_byte(0x19);
        let warm_key = Word::from(1);
        let cold_key = Word::from(2);
        let mut database = CacheDB::default();
        database.insert_account_info(&account, AccountInfo::default().with_balance(Word::from(1)));
        database.insert_account_storage(&account, &warm_key, &Word::from(3));
        database.insert_account_storage(&account, &cold_key, &Word::from(4));
        let mut state = State::new(database);

        state.prewarm_storage_slot(&account, warm_key);
        state.storage_slot(&account, cold_key, false).unwrap().write(Word::from(5));

        state.storage(&account).wipe();
        assert!(state.storage_slot(&account, warm_key, false).unwrap().is_warm());
        assert!(!state.storage_slot(&account, cold_key, false).unwrap().is_warm());
        assert_eq!(state.storage_slot(&account, warm_key, false).unwrap().current(), Word::ZERO);
        assert_eq!(state.storage_slot(&account, cold_key, false).unwrap().current(), Word::ZERO);

        let pending = state.take_pending_state();
        let overlay = pending.storage.get(&account).expect("wipe must be emitted");
        assert!(overlay.wiped);
        assert!(overlay.changed_slots().next().is_none());
    }

    #[test]
    fn journaled_storage_mutations_journal_and_roll_back() {
        let address = Address::from([0x33; 20]);
        let key = Word::from(1);
        let mut database = CacheDB::default();
        database.insert_account_info(&address, AccountInfo::default());
        database.insert_account_storage(&address, &key, &Word::from(10));
        let mut state = State::new(database);

        let checkpoint = state.checkpoint();
        {
            let mut slot = state.storage(&address).into_slot(key, false).unwrap();
            assert_eq!(slot.current(), Word::from(10));
            assert_eq!(slot.original(), Word::from(10));
            assert!(slot.warm(), "first access is cold");
            assert!(!slot.warm(), "second access is warm");
            slot.set(Word::from(20));
            slot.set(Word::from(30));
        }

        assert!(state.storage_slot(&address, key, false).unwrap().is_warm());
        assert_eq!(state.storage_slot(&address, key, false).unwrap().current(), Word::from(30));

        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(!state.storage_slot(&address, key, false).unwrap().is_warm());
        assert_eq!(state.storage_slot(&address, key, false).unwrap().current(), Word::from(10));
        assert!(!state.take_pending_state().is_changed());
    }

    #[test]
    fn journaled_storage_read_only_handle_journals_nothing() {
        let address = Address::from([0x34; 20]);
        let key = Word::from(7);
        let mut database = CacheDB::default();
        database.insert_account_info(&address, AccountInfo::default());
        database.insert_account_storage(&address, &key, &Word::from(5));
        let mut state = State::new(database);

        let checkpoint = state.checkpoint();
        {
            let slot = state.storage(&address).into_slot(key, false).unwrap();
            assert_eq!(slot.current(), Word::from(5));
        }
        // Loading caches the value but a read-only handle records no transition.
        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(!state.take_pending_state().is_changed());
    }

    #[test]
    fn storage_handle_reports_warmth_before_loading() {
        let address = Address::from([0x35; 20]);
        let key = Word::from(9);
        let mut database = CacheDB::default();
        database.insert_account_info(&address, AccountInfo::default());
        database.insert_account_storage(&address, &key, &Word::from(42));
        let mut state = State::new(database);

        // A cold, not-yet-loaded slot reports neither loaded nor warm without touching the
        // database.
        assert!(!state.storage(&address).is_loaded(&key));
        assert!(!state.storage(&address).is_warm(&key));

        // Loading materializes the slot with its database value; warming marks it warm.
        {
            let mut slot = state.storage(&address).into_slot(key, false).unwrap();
            assert_eq!(slot.current(), Word::from(42));
            assert!(slot.warm(), "first access is cold");
            assert!(!slot.warm(), "second access is warm");
        }
        assert!(state.storage(&address).is_loaded(&key));
        assert!(state.storage(&address).is_warm(&key));
    }
}
