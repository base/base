//! Block-level state accumulation.

use alloc::vec::Vec;
use core::convert::Infallible;

use alloy_primitives::{
    Address, B256,
    map::{AddressMap, AddressSet, B256Map, hash_map},
};

use super::{
    AccountChangeRef, AccountInfo, StateChangeSink, StateChangeSource, StorageChange, Tracked,
};
use crate::{
    bytecode::Bytecode,
    interpreter::Word,
    storage_key::{StorageKey, StorageKeyMap},
};

/// Mutable block-level state accumulator.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockStateAccumulator {
    accounts: AddressMap<Tracked<Option<AccountInfo>>>,
    storage_wipes: AddressSet,
    storage: StorageKeyMap<Tracked<Word>>,
    code: B256Map<Bytecode>,
}

impl BlockStateAccumulator {
    /// Creates an empty block state accumulator.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether the accumulator contains no state changes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
            && self.storage_wipes.is_empty()
            && self.storage.is_empty()
            && self.code.is_empty()
    }

    /// Returns account deltas with addresses in arbitrary map order.
    #[inline]
    pub fn accounts(&self) -> impl Iterator<Item = (Address, &Tracked<Option<AccountInfo>>)> {
        self.accounts.iter().map(|(&address, delta)| (address, delta))
    }

    /// Returns storage-wipe addresses in arbitrary set order.
    #[inline]
    pub fn storage_wipes(&self) -> impl Iterator<Item = Address> + '_ {
        self.storage_wipes.iter().copied()
    }

    /// Returns storage deltas with storage keys in arbitrary map order.
    ///
    /// If a slot's address appears in [`Self::storage_wipes`], consumers should apply the wipe
    /// before this slot and treat [`Tracked::current`] as the value to write after the wipe.
    #[inline]
    pub fn storage(&self) -> impl Iterator<Item = (StorageKey, &Tracked<Word>)> {
        self.storage.iter().map(|(&key, delta)| (key, delta))
    }

    /// Returns bytecode entries in arbitrary map order.
    #[inline]
    pub fn code(&self) -> impl Iterator<Item = (&B256, &Bytecode)> {
        self.code.iter()
    }

    /// Returns account deltas with addresses sorted by address.
    pub fn accounts_sorted(&self) -> Vec<(Address, &Tracked<Option<AccountInfo>>)> {
        let mut accounts = self.accounts().collect::<Vec<_>>();
        accounts.sort_by_key(|(address, _)| *address);
        accounts
    }

    /// Returns storage-wipe addresses sorted by address.
    pub fn storage_wipes_sorted(&self) -> Vec<Address> {
        let mut storage_wipes = self.storage_wipes().collect::<Vec<_>>();
        storage_wipes.sort_unstable();
        storage_wipes
    }

    /// Returns storage deltas with storage keys sorted by address and slot.
    pub fn storage_sorted(&self) -> Vec<(StorageKey, &Tracked<Word>)> {
        let mut storage = self.storage().collect::<Vec<_>>();
        storage.sort_by_key(|(key, _)| (key.address(), key.key()));
        storage
    }

    /// Returns bytecode entries sorted by code hash.
    ///
    /// The counterpart to [`Self::accounts_sorted`], [`Self::storage_sorted`], and
    /// [`Self::storage_wipes_sorted`]: [`Self::code`] iterates a map, so a consumer
    /// that needs a deterministic enumeration of the whole accumulator would
    /// otherwise have to sort this one collection itself.
    pub fn code_sorted(&self) -> Vec<(&B256, &Bytecode)> {
        let mut code = self.code().collect::<Vec<_>>();
        code.sort_unstable_by_key(|(hash, _)| *hash);
        code
    }
}

impl StateChangeSink for BlockStateAccumulator {
    type Error = Infallible;

    #[inline]
    fn bytecode(&mut self, code_hash: B256, code: &Bytecode) -> Result<(), Self::Error> {
        self.code.entry(code_hash).or_insert_with(|| code.clone());
        Ok(())
    }

    fn account(&mut self, change: AccountChangeRef<'_>) -> Result<(), Self::Error> {
        let original = change.original.map(AccountInfo::clone_no_code);
        let current = change.current.map(AccountInfo::clone_no_code);
        let deletes_account = current.is_none();

        match self.accounts.entry(change.address) {
            hash_map::Entry::Occupied(mut entry) => {
                let delta = entry.get_mut();
                // Reviving an account deleted earlier in the block re-records the deletion's
                // implied storage wipe: the deletion dropped the wipe marker because applying
                // `current == None` wipes storage at the sink, but applying a live account does
                // not, so pre-block storage would otherwise leak through the revival.
                if delta.current.is_none() && current.is_some() && delta.original.is_some() {
                    self.storage_wipes.insert(change.address);
                }
                delta.set_current(current);
                if !delta.is_changed() {
                    entry.remove();
                }
            }
            hash_map::Entry::Vacant(entry) => {
                if original != current {
                    entry.insert(Tracked::from_parts(original, current));
                }
            }
        }

        if deletes_account {
            self.storage_wipes.remove(&change.address);
            self.storage.retain(|key, _| key.address() != change.address);
        } else if self.accounts.get(&change.address).is_some_and(|delta| delta.original.is_none()) {
            self.storage_wipes.remove(&change.address);
        }
        Ok(())
    }

    fn storage_wipe(&mut self, address: Address) -> Result<(), Self::Error> {
        let record_wipe = self.accounts.get(&address).is_none_or(|delta| delta.original.is_some());
        if record_wipe {
            self.storage_wipes.insert(address);
        }
        self.storage.retain(|key, _| key.address() != address);
        Ok(())
    }

    fn storage(&mut self, change: StorageChange) -> Result<(), Self::Error> {
        let storage_key = StorageKey::new(change.address, change.key);
        let storage_wiped = self.storage_wipes.contains(&change.address);
        match self.storage.entry(storage_key) {
            hash_map::Entry::Occupied(mut entry) => {
                let delta = entry.get_mut();
                delta.set_current(change.current);
                if (storage_wiped && delta.current.is_zero())
                    || (!storage_wiped && !delta.is_changed())
                {
                    entry.remove();
                }
            }
            hash_map::Entry::Vacant(entry) => {
                if (storage_wiped && change.current.is_zero())
                    || (!storage_wiped && change.original == change.current)
                {
                    return Ok(());
                }
                entry.insert(Tracked::from_parts(change.original, change.current));
            }
        }
        Ok(())
    }
}

impl StateChangeSource for BlockStateAccumulator {
    #[inline]
    fn visit<S: StateChangeSink>(&self, sink: &mut S) -> Result<(), S::Error> {
        visit_block_changes(&self.accounts, &self.storage_wipes, &self.storage, &self.code, sink)
    }
}

fn visit_block_changes<S: StateChangeSink>(
    accounts: &AddressMap<Tracked<Option<AccountInfo>>>,
    storage_wipes: &AddressSet,
    storage: &StorageKeyMap<Tracked<Word>>,
    code: &B256Map<Bytecode>,
    sink: &mut S,
) -> Result<(), S::Error> {
    let mut code_entries = code.iter().collect::<Vec<_>>();
    code_entries.sort_by_key(|(code_hash, _)| **code_hash);
    for (&code_hash, code) in code_entries {
        sink.bytecode(code_hash, code)?;
    }

    let mut storage_wipes = storage_wipes.iter().copied().collect::<Vec<_>>();
    storage_wipes.sort_unstable();
    for address in &storage_wipes {
        sink.storage_wipe(*address)?;
    }

    let mut storage_deltas = storage.iter().collect::<Vec<_>>();
    storage_deltas.sort_by_key(|entry| (entry.0.address(), entry.0.key()));
    for (key, delta) in storage_deltas {
        sink.storage(StorageChange {
            address: key.address(),
            key: key.key(),
            original: delta.original,
            current: delta.current,
        })?;
    }

    let mut account_deltas = accounts.iter().collect::<Vec<_>>();
    account_deltas.sort_by_key(|entry| *entry.0);
    for (address, delta) in account_deltas {
        sink.account(AccountChangeRef {
            address: *address,
            original: delta.original.as_ref(),
            current: delta.current.as_ref(),
            // Block-level aggregation loses per-transaction lifecycle flags.
            created: false,
            selfdestructed: false,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    #[cfg(feature = "serde")]
    use alloy_primitives::B256;
    use alloy_primitives::{Address, map::U256Map};

    use super::{
        super::{
            Account, AccountInfo, PendingState, StateChangeSink, StateChangeSource, StorageOverlay,
            StorageSlot, Tracked,
        },
        BlockStateAccumulator,
    };
    #[cfg(feature = "serde")]
    use crate::bytecode::Bytecode;
    use crate::interpreter::Word;

    #[test]
    fn code_sorted_orders_by_hash() {
        use alloy_primitives::{B256, Bytes};

        use crate::bytecode::Bytecode;

        let mut block = BlockStateAccumulator::new();
        // Inserted descending, so map order cannot be mistaken for sorted order.
        for byte in [0xcc_u8, 0x11, 0x77] {
            block
                .bytecode(
                    B256::repeat_byte(byte),
                    &Bytecode::new_raw_checked(Bytes::from(vec![byte])).unwrap(),
                )
                .unwrap();
        }

        let hashes: Vec<_> = block.code_sorted().iter().map(|(hash, _)| **hash).collect();
        assert_eq!(
            hashes,
            vec![B256::repeat_byte(0x11), B256::repeat_byte(0x77), B256::repeat_byte(0xcc)]
        );
        // Same entries as the unsorted accessor, only ordered.
        assert_eq!(block.code_sorted().len(), block.code().count());
    }

    fn changes(
        address: Address,
        original: Option<AccountInfo>,
        current: Option<AccountInfo>,
        wiped: bool,
        slots: U256Map<StorageSlot>,
    ) -> PendingState {
        let mut pending = PendingState::default();
        pending.accounts.insert(address, Account::new(original, current));
        if wiped || !slots.is_empty() {
            pending.storage.insert(address, StorageOverlay { wiped, slots, _non_exhaustive: () });
        }
        pending
    }

    fn slot(key: Word, original: Word, current: Word) -> U256Map<StorageSlot> {
        U256Map::from_iter([(
            key,
            StorageSlot {
                value: Tracked::from_parts(original, current),
                is_warm: false,
                _non_exhaustive: (),
            },
        )])
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_binary_roundtrip() {
        let mut accumulator = BlockStateAccumulator::new();
        let code_hash = B256::with_last_byte(1);
        let bytecode = Bytecode::new_raw_checked(vec![0x60, 0x00].into()).unwrap();
        accumulator.bytecode(code_hash, &bytecode).unwrap();

        let encoded = postcard::to_allocvec(&accumulator).unwrap();
        let deserialized: BlockStateAccumulator = postcard::from_bytes(&encoded).unwrap();

        assert_eq!(deserialized, accumulator);
    }

    #[test]
    fn block_accumulator_collapses_create_then_delete() {
        let address = Address::from([0x50; 20]);
        let key = Word::from(1);
        let created = AccountInfo::default().with_nonce(1);
        let mut accumulator = BlockStateAccumulator::new();

        let create = changes(
            address,
            None,
            Some(created.clone()),
            true,
            slot(key, Word::ZERO, Word::from(7)),
        );
        create.visit(&mut accumulator).expect("block accumulator is infallible");

        let delete = changes(address, Some(created), None, true, U256Map::default());
        delete.visit(&mut accumulator).expect("block accumulator is infallible");

        assert!(accumulator.accounts_sorted().is_empty());
        assert!(accumulator.storage_wipes_sorted().is_empty());
        assert!(accumulator.storage_sorted().is_empty());
    }

    #[test]
    fn block_accumulator_preserves_original_for_delete_then_recreate() {
        let address = Address::from([0x51; 20]);
        let key = Word::from(1);
        let original = AccountInfo::default().with_balance(Word::from(3));
        let recreated = AccountInfo::default().with_nonce(1);
        let mut accumulator = BlockStateAccumulator::new();

        let delete = changes(address, Some(original.clone()), None, true, U256Map::default());
        delete.visit(&mut accumulator).expect("block accumulator is infallible");

        let create = changes(
            address,
            None,
            Some(recreated.clone()),
            true,
            slot(key, Word::ZERO, Word::from(7)),
        );
        create.visit(&mut accumulator).expect("block accumulator is infallible");

        let accounts = accumulator.accounts_sorted();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].1.original.as_ref(), Some(&original));
        assert_eq!(accounts[0].1.current.as_ref(), Some(&recreated));
        assert_eq!(accumulator.storage_wipes_sorted(), [address]);

        let storage = accumulator.storage_sorted();
        assert_eq!(storage.len(), 1);
        assert_eq!(storage[0].0.key(), key);
        assert_eq!(storage[0].1.current, Word::from(7));
    }

    #[test]
    fn block_accumulator_restores_wipe_when_deleted_account_is_revived() {
        // Selfdestruct followed by a revival in a later transaction of the same block (a plain
        // transfer or a re-create) must keep the deletion's storage wipe: the revived account is
        // applied as a live account, which does not wipe storage at the sink by itself.
        let address = Address::from([0x55; 20]);
        let original = AccountInfo::default().with_balance(Word::from(3));
        let revived = AccountInfo::default().with_balance(Word::from(1));
        let mut accumulator = BlockStateAccumulator::new();

        let delete = changes(address, Some(original.clone()), None, true, U256Map::default());
        delete.visit(&mut accumulator).expect("block accumulator is infallible");
        assert!(accumulator.storage_wipes_sorted().is_empty(), "deletion subsumes the wipe");

        let revive = changes(address, None, Some(revived.clone()), false, U256Map::default());
        revive.visit(&mut accumulator).expect("block accumulator is infallible");

        let accounts = accumulator.accounts_sorted();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].1.original.as_ref(), Some(&original));
        assert_eq!(accounts[0].1.current.as_ref(), Some(&revived));
        assert_eq!(accumulator.storage_wipes_sorted(), [address]);

        // Reviving back to a value equal to the original collapses the account delta, but the
        // wipe still stands: the account's pre-block storage was destroyed.
        let mut collapsing = BlockStateAccumulator::new();
        changes(address, Some(original.clone()), None, true, U256Map::default())
            .visit(&mut collapsing)
            .expect("block accumulator is infallible");
        changes(address, None, Some(original), false, U256Map::default())
            .visit(&mut collapsing)
            .expect("block accumulator is infallible");
        assert!(collapsing.accounts_sorted().is_empty());
        assert_eq!(collapsing.storage_wipes_sorted(), [address]);
    }

    #[test]
    fn block_accumulator_keeps_nonzero_write_after_storage_wipe() {
        let address = Address::from([0x52; 20]);
        let key = Word::from(1);
        let original = AccountInfo::default().with_balance(Word::from(3));
        let mut accumulator = BlockStateAccumulator::new();

        let wipe_and_restore = changes(
            address,
            Some(original.clone()),
            Some(original),
            true,
            slot(key, Word::ZERO, Word::from(5)),
        );
        wipe_and_restore.visit(&mut accumulator).expect("block accumulator is infallible");

        assert!(accumulator.accounts_sorted().is_empty());
        assert_eq!(accumulator.storage_wipes_sorted(), [address]);

        let storage = accumulator.storage_sorted();
        assert_eq!(storage.len(), 1);
        assert_eq!(storage[0].0.key(), key);
        assert_eq!(storage[0].1.current, Word::from(5));
    }

    #[test]
    fn block_accumulator_deletion_subsumes_storage_writes() {
        let address = Address::from([0x56; 20]);
        let key = Word::from(1);
        let original = AccountInfo::default().with_balance(Word::from(3));
        let mut accumulator = BlockStateAccumulator::new();

        let delete =
            changes(address, Some(original), None, true, slot(key, Word::from(5), Word::from(7)));
        delete.visit(&mut accumulator).expect("block accumulator is infallible");

        let accounts = accumulator.accounts_sorted();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].0, address);
        assert!(accounts[0].1.current.is_none());
        assert!(accumulator.storage_wipes_sorted().is_empty());
        assert!(accumulator.storage_sorted().is_empty());
    }

    #[test]
    fn block_accumulator_collapses_storage_wipe_write_wipe() {
        let address = Address::from([0x52; 20]);
        let key = Word::from(1);
        let mut accumulator = BlockStateAccumulator::new();

        let first = changes(address, None, None, true, slot(key, Word::from(5), Word::from(7)));
        first.visit(&mut accumulator).expect("block accumulator is infallible");
        changes(address, None, None, true, U256Map::default())
            .visit(&mut accumulator)
            .expect("block accumulator is infallible");

        assert!(accumulator.accounts_sorted().is_empty());
        assert_eq!(accumulator.storage_wipes_sorted(), [address]);
        assert!(accumulator.storage_sorted().is_empty());
    }

    #[test]
    fn block_accumulator_keeps_account_only_and_storage_only_changes_separate() {
        let account_address = Address::from([0x53; 20]);
        let storage_address = Address::from([0x54; 20]);
        let key = Word::from(1);
        let original = AccountInfo::default().with_balance(Word::from(1));
        let current = AccountInfo::default().with_balance(Word::from(2));
        let mut accumulator = BlockStateAccumulator::new();

        changes(
            account_address,
            Some(original.clone()),
            Some(current.clone()),
            false,
            U256Map::default(),
        )
        .visit(&mut accumulator)
        .expect("block accumulator is infallible");
        changes(storage_address, None, None, false, slot(key, Word::from(3), Word::from(4)))
            .visit(&mut accumulator)
            .expect("block accumulator is infallible");

        let accounts = accumulator.accounts_sorted();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].0, account_address);
        assert_eq!(accounts[0].1.original.as_ref(), Some(&original));
        assert_eq!(accounts[0].1.current.as_ref(), Some(&current));
        assert!(accumulator.storage_wipes_sorted().is_empty());

        let storage = accumulator.storage_sorted();
        assert_eq!(storage.len(), 1);
        assert_eq!(storage[0].0.address(), storage_address);
        assert_eq!(storage[0].0.key(), key);
        assert_eq!(storage[0].1.original, Word::from(3));
        assert_eq!(storage[0].1.current, Word::from(4));
    }
}
