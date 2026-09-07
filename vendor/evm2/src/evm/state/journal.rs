//! Revert journal and checkpoint types.

use alloy_primitives::Address;

use super::AccountInfo;
use crate::interpreter::Word;

/// State checkpoint for reverting state changes.
#[allow(missing_copy_implementations)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateCheckpoint {
    /// Revert journal length at the checkpoint.
    pub(crate) journal_len: usize,
    /// Emitted log count at the checkpoint.
    pub(crate) logs_len: usize,
}

impl StateCheckpoint {
    /// Creates a checkpoint from journal and log cursors.
    #[inline]
    pub const fn new(journal_len: usize, logs_len: usize) -> Self {
        Self { journal_len, logs_len }
    }

    /// Returns the revert-journal cursor captured by this checkpoint.
    #[inline]
    pub const fn journal_len(&self) -> usize {
        self.journal_len
    }

    /// Returns the emitted-log cursor captured by this checkpoint.
    #[inline]
    pub const fn logs_len(&self) -> usize {
        self.logs_len
    }
}

/// Compact journal entry for reverting state changes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JournalEntry {
    /// Account overlay snapshot recorded before the first mutation made through an
    /// [`AccountHandle`](super::AccountHandle), reverting the present account value and all
    /// per-account flags in one entry.
    AccountChange {
        /// Account address.
        address: Address,
        /// Previous present account value.
        previous: Option<AccountInfo>,
        /// Previous warm flag.
        previous_is_warm: bool,
        /// Previous touched flag.
        previous_is_touched: bool,
        /// Previous self-destructed flag.
        previous_is_destroyed: bool,
        /// Previous created-in-transaction flag.
        previous_just_created: bool,
        /// Previous code-changed flag.
        previous_code_changed: bool,
    },
    /// Persistent storage changed.
    StorageChange {
        /// Account address.
        address: Address,
        /// Storage key.
        key: Word,
        /// Previous current storage value.
        previous: Word,
    },
    /// Transient storage changed.
    TransientStorageChange {
        /// Account address.
        address: Address,
        /// Storage key.
        key: Word,
        /// Previous transient storage value.
        previous: Option<Word>,
    },
    /// Storage slot was warmed by EIP-2929 access tracking.
    StorageWarmed {
        /// Account address.
        address: Address,
        /// Storage key.
        key: Word,
    },
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::Log;

    use super::*;
    use crate::{
        SpecId, Version,
        evm::{
            CacheDB,
            state::{AccountInfo, State},
        },
    };

    #[test]
    fn destruct_change_rolls_back_to_checkpoint() {
        let address = Address::from([0x33; 20]);
        let mut state = State::new(CacheDB::default());

        let checkpoint = state.checkpoint();
        state.account(&address, false).unwrap().mark_destructed();

        assert!(state.account(&address, false).unwrap().is_destructed());
        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(!state.account(&address, false).unwrap().is_destructed());
    }

    #[test]
    fn log_rolls_back_to_checkpoint() {
        use alloy_primitives::{Bytes, LogData};

        let mut state = State::new(CacheDB::default());
        let kept = Log {
            address: Address::from([0x44; 20]),
            data: LogData::new_unchecked(Vec::new(), Bytes::from_static(&[0x01])),
        };
        let reverted = Log {
            address: Address::from([0x55; 20]),
            data: LogData::new_unchecked(Vec::new(), Bytes::from_static(&[0x02])),
        };

        state.log(kept.clone());
        let checkpoint = state.checkpoint();
        state.log(reverted);

        assert_eq!(
            state.logs(),
            &[
                kept.clone(),
                Log {
                    address: Address::from([0x55; 20]),
                    data: LogData::new_unchecked(Vec::new(), Bytes::from_static(&[0x02])),
                }
            ]
        );
        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert_eq!(state.logs(), &[kept]);
    }

    #[test]
    fn spurious_dragon_rollback_preserves_precompile3_touch() {
        let precompile3 = Address::with_last_byte(3);
        let other = Address::with_last_byte(4);
        let mut database = CacheDB::default();
        database.insert_account_info(&precompile3, AccountInfo::default());
        database.insert_account_info(&other, AccountInfo::default());
        let mut state = State::new(database);

        let checkpoint = state.checkpoint();
        state.account(&precompile3, false).unwrap().touch();
        state.account(&other, false).unwrap().touch();

        state.rollback(checkpoint, Version::base(SpecId::SPURIOUS_DRAGON).features);
        assert!(state.account(&precompile3, false).unwrap().is_touched());
        assert!(!state.account(&other, false).unwrap().is_touched());
    }

    #[test]
    fn non_revertible_warmth_is_not_journaled_or_rolled_back() {
        let base_account = Address::with_last_byte(0x10);
        let frame_account = Address::with_last_byte(0x11);
        let base_storage = Address::with_last_byte(0x12);
        let frame_storage = Address::with_last_byte(0x13);
        let key = Word::from(1);
        let mut state = State::new(CacheDB::default());

        state.prewarm(&base_account);
        state.prewarm_storage_slot(&base_storage, key);
        assert!(state.journal.is_empty());

        let checkpoint = state.checkpoint();
        assert!(state.account(&frame_account, false).unwrap().warm());
        assert!(state.storage_slot(&frame_storage, key, false).unwrap().warm());
        // The load itself is an un-journaled read cache; warming the frame account records an
        // AccountChange and warming the slot records StorageWarmed: two revertible entries in
        // total.
        assert_eq!(state.journal.len(), 2);

        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(state.account(&base_account, false).unwrap().is_warm());
        assert!(state.storage_slot(&base_storage, key, false).unwrap().is_warm());
        assert!(!state.account(&frame_account, false).unwrap().is_warm());
        assert!(!state.storage_slot(&frame_storage, key, false).unwrap().is_warm());
    }

    #[test]
    fn warm_only_entries_do_not_emit_state_changes() {
        let account = Address::with_last_byte(0x14);
        let storage_account = Address::with_last_byte(0x15);
        let key = Word::from(1);
        let mut state = State::new(CacheDB::default());

        state.prewarm(&account);
        state.prewarm_storage_slot(&storage_account, key);

        let pending = state.take_pending_state();
        assert!(pending.is_empty());
        assert!(state.account(&account, false).unwrap().is_warm());
        assert!(state.storage_slot(&storage_account, key, false).unwrap().is_warm());

        state.clear_transaction_state();
        assert!(!state.account(&account, false).unwrap().is_warm());
        assert!(!state.storage_slot(&storage_account, key, false).unwrap().is_warm());
    }

    #[test]
    fn rollback_preserves_non_revertible_account_warmth_after_load() {
        let account = Address::with_last_byte(0x16);
        let mut database = CacheDB::default();
        database.insert_account_info(&account, AccountInfo::default().with_balance(Word::from(1)));
        let mut state = State::new(database);

        state.prewarm(&account);
        let checkpoint = state.checkpoint();
        assert!(state.account(&account, false).unwrap().exists());
        assert!(state.account(&account, false).unwrap().get().is_some());

        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        // The load is an un-journaled read cache, so it survives rollback. Warmth comes from the
        // non-revertible base warm set, and the unchanged cached entry emits no state change.
        assert!(state.prewarm_set().is_warm(&account));
        assert!(state.account(&account, false).unwrap().is_warm());
        assert!(!state.take_pending_state().is_changed());
    }

    #[test]
    fn rollback_preserves_non_revertible_storage_warmth_after_write() {
        let account = Address::with_last_byte(0x17);
        let key = Word::from(1);
        let mut database = CacheDB::default();
        database.insert_account_info(&account, AccountInfo::default().with_balance(Word::from(1)));
        let mut state = State::new(database);

        state.prewarm_storage_slot(&account, key);
        let checkpoint = state.checkpoint();
        state.storage(&account).into_slot(key, false).unwrap().write(Word::from(7));
        assert_eq!(state.storage_slot(&account, key, false).unwrap().current(), Word::from(7));

        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(state.storage_slot(&account, key, false).unwrap().is_warm());
        assert!(!state.take_pending_state().is_changed());
    }

    #[test]
    fn rollback_reverts_storage_warmth_without_discarding_cached_value() {
        let account = Address::with_last_byte(0x18);
        let key = Word::from(1);
        let value = Word::from(9);
        let mut database = CacheDB::default();
        database.insert_account_info(&account, AccountInfo::default().with_balance(Word::from(1)));
        database.insert_account_storage(&account, &key, &value);
        let mut state = State::new(database);

        let checkpoint = state.checkpoint();
        assert!(state.storage_slot(&account, key, false).unwrap().warm());
        assert_eq!(state.storage(&account).into_slot(key, false).unwrap().current(), value);

        state.rollback(checkpoint, Version::base(SpecId::FRONTIER).features);
        assert!(!state.storage_slot(&account, key, false).unwrap().is_warm());
        assert_eq!(state.storage_slot(&account, key, false).unwrap().current(), value);
        assert!(!state.take_pending_state().is_changed());

        assert!(state.storage_slot(&account, key, false).unwrap().warm());
    }
}
