use alloc::collections::{BTreeMap, BTreeSet};
use core::ops::RangeBounds;

use alloy_primitives::{Address, B256, BlockNumber, map::B256Map};
use auto_impl::auto_impl;
use base_execution_evm_runtime::StoredAccount as Account;

use crate::{AccountBeforeTx, BlockNumberAddress, ProviderResult, StorageEntry};

/// Hashing Writer
#[auto_impl(&, Arc, Box)]
pub trait HashingWriter: Send {
    /// Unwind and clear account hashing.
    ///
    /// # Returns
    ///
    /// Set of hashed keys of updated accounts.
    fn unwind_account_hashing<'a>(
        &self,
        changesets: impl Iterator<Item = &'a (BlockNumber, AccountBeforeTx)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>>;

    /// Unwind and clear account hashing in a given block range.
    ///
    /// # Returns
    ///
    /// Set of hashed keys of updated accounts.
    fn unwind_account_hashing_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>>;

    /// Inserts all accounts into [`AccountsHistory`][base_execution_state_database::tables::AccountsHistory] table.
    ///
    /// # Returns
    ///
    /// Set of hashed keys of updated accounts.
    fn insert_account_for_hashing(
        &self,
        accounts: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>>;

    /// Unwind and clear storage hashing.
    ///
    /// # Returns
    ///
    /// Mapping of hashed keys of updated accounts to their respective updated hashed slots.
    fn unwind_storage_hashing(
        &self,
        changesets: impl Iterator<Item = (BlockNumberAddress, StorageEntry)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>>;

    /// Unwind and clear storage hashing in a given block range.
    ///
    /// # Returns
    ///
    /// Mapping of hashed keys of updated accounts to their respective updated hashed slots.
    fn unwind_storage_hashing_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>>;

    /// Iterates over storages and inserts them to hashing table.
    ///
    /// # Returns
    ///
    /// Mapping of hashed keys of updated accounts to their respective updated hashed slots.
    fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = StorageEntry>)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>>;
}
