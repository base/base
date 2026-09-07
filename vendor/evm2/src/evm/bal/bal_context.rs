//! EIP-7928 Block Access List state carried alongside the accepted-overlay database.

use alloc::sync::Arc;
use core::convert::Infallible;

use alloy_eip7928::BlockAccessList;
use alloy_primitives::{Address, map::AddressMap};

use super::{AccountBal, Bal, BalError, BlockAccessIndex};
use crate::{
    AnyError, ErrorCode,
    evm::state::{
        Account, AccountChangeRef, AccountInfo, PendingState, StateChangeSink, StorageChange,
        StorageOverlay,
    },
    interpreter::Word,
};

/// Result of an EIP-7928 BAL lookup during a read.
type BalResult<T> = Result<T, BalError>;

/// EIP-7928 Block Access List state: an attached BAL consulted on reads plus an optional builder
/// that accumulates one from executed transactions, both keyed at a shared block access index.
///
/// This bundles the read and write BAL machinery so the database wrapper that carries it (evm2's
/// [`CacheDB`](crate::evm::CacheDB)) is not itself BAL-oriented; the state lives on the database
/// wrapper rather than the journaled state.
///
/// The two roles are independent:
///
/// - **Reads** ([`Self::bal`]): when an attached BAL is present, [`Self::get_bal_account`] /
///   [`Self::populate_bal_account`] and [`Self::bal_storage`] serve account info and storage from
///   it at [`Self::bal_index`] (post-state per transaction). A read not covered by the BAL is
///   either an error or falls through to the database, depending on whether fallback is enabled.
/// - **Writes** ([`Self::bal_builder`]): when enabled, `Self::commit_pending` folds each committed
///   transaction's pending post-state into the builder at [`Self::bal_index`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalContext {
    /// Optional attached EIP-7928 BAL consulted on reads.
    ///
    /// `None` (the default) disables BAL-served reads, so reads go straight to the cache/database.
    /// When `Some`, reads resolve account info and storage from the BAL at [`Self::bal_index`].
    /// Shared via [`Arc`] so the same BAL can back multiple executions.
    bal: Option<Arc<Bal>>,
    /// Optional EIP-7928 Block Access List builder.
    ///
    /// `None` (the default) disables BAL construction so normal execution pays nothing. When
    /// `Some`, [`Self::commit_pending`] folds each committed transaction's post-state into it.
    bal_builder: Option<Bal>,
    /// Current EIP-7928 block access index used by both BAL-served reads and
    /// [`Self::commit_pending`].
    ///
    /// Callers bump this once per transaction (see [`Self::bump_bal_index`]) so each transaction's
    /// writes are recorded under, and reads served at, a distinct index.
    bal_index: BlockAccessIndex,
    /// Whether reads not covered by the attached [`Self::bal`] fall back to the cache/database
    /// instead of returning [`ErrorCode::BAL_NOT_COVERED`].
    ///
    /// During block validation an access outside the BAL means the BAL is invalid, so this
    /// defaults to `false`. Enabling it allows executing transactions that are not part of the
    /// block (e.g. RPC calls) on top of BAL-positioned state.
    allow_db_fallback: bool,
    /// Last BAL lookup error, surfaced through [`Self::take_error`] after a read returns
    /// [`ErrorCode::BAL_NOT_COVERED`].
    bal_error: Option<BalError>,
}

impl BalContext {
    /// Creates an empty context with no attached BAL and no builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches an EIP-7928 BAL consulted on reads, and returns `self`.
    #[inline]
    pub fn with_bal(mut self, bal: Arc<Bal>) -> Self {
        self.bal = Some(bal);
        self
    }

    /// Attaches an EIP-7928 BAL consulted on reads.
    #[inline]
    pub fn set_bal(&mut self, bal: Arc<Bal>) {
        self.bal = Some(bal);
    }

    /// Returns the attached read BAL, or `None` when no BAL is attached.
    #[inline]
    pub const fn bal(&self) -> Option<&Arc<Bal>> {
        self.bal.as_ref()
    }

    /// Detaches the read BAL, so reads resolve from the cache/database again.
    ///
    /// The counterpart to [`Self::set_bal`]: without it an attached BAL cannot be
    /// removed, and a caller wanting unrestricted reads has to attach an empty one
    /// with [`Self::set_allow_db_fallback`] enabled to get the same behavior.
    #[inline]
    pub fn clear_bal(&mut self) {
        self.bal = None;
    }

    /// Sets whether reads not covered by the attached BAL fall back to the cache/database instead
    /// of returning [`ErrorCode::BAL_NOT_COVERED`], and returns `self`.
    #[inline]
    pub const fn with_allow_db_fallback(mut self, allow: bool) -> Self {
        self.allow_db_fallback = allow;
        self
    }

    /// Sets whether reads not covered by the attached BAL fall back to the cache/database.
    #[inline]
    pub const fn set_allow_db_fallback(&mut self, allow: bool) {
        self.allow_db_fallback = allow;
    }

    /// Enables EIP-7928 BAL construction, installing an empty builder, and returns `self`.
    #[inline]
    pub fn with_bal_builder(mut self) -> Self {
        self.bal_builder = Some(Bal::new());
        self
    }

    /// Enables EIP-7928 BAL construction in place, installing an empty builder.
    #[inline]
    pub fn enable_bal_builder(&mut self) {
        self.bal_builder = Some(Bal::new());
    }

    /// Returns the in-progress BAL builder, or `None` when BAL construction is disabled.
    #[inline]
    pub const fn bal_builder(&self) -> Option<&Bal> {
        self.bal_builder.as_ref()
    }

    /// Returns whether BAL construction is enabled.
    #[inline]
    pub const fn has_builder(&self) -> bool {
        self.bal_builder.is_some()
    }

    /// Returns the current EIP-7928 block access index.
    #[inline]
    pub const fn bal_index(&self) -> BlockAccessIndex {
        self.bal_index
    }

    /// Resets the block access index to the pre-execution slot (index `0`).
    ///
    /// Call this before executing a block's transactions.
    #[inline]
    pub const fn reset_bal_index(&mut self) {
        self.bal_index = BlockAccessIndex::PRE_EXECUTION;
    }

    /// Sets the block access index to the given value.
    #[inline]
    pub const fn set_bal_index(&mut self, index: BlockAccessIndex) {
        self.bal_index = index;
    }

    /// Bumps the block access index by one.
    ///
    /// Call this once per transaction so each transaction's writes are recorded under a distinct
    /// index, matching the EIP-7928 layout where transaction `i` maps to index `i + 1`.
    #[inline]
    pub const fn bump_bal_index(&mut self) {
        self.bal_index.increment();
    }

    /// Folds a detached [`PendingState`] -- a committed transaction's post-state -- into the BAL
    /// builder at the current [`Self::bal_index`].
    ///
    /// No-op when BAL construction is disabled. Loaded-but-unchanged accounts and storage slots
    /// are recorded as BAL reads; changed ones as writes.
    #[inline]
    pub fn commit_pending(&mut self, pending: &PendingState) {
        self.commit(&pending.accounts, &pending.storage);
    }

    /// Folds a committed transaction's pending accounts and storage overlays into the BAL builder
    /// at the current [`Self::bal_index`].
    ///
    /// Same as [`Self::commit_pending`], operating on the transaction layers directly so the
    /// overlay need not be detached. No-op when BAL construction is disabled.
    #[inline]
    pub(crate) fn commit(
        &mut self,
        accounts: &AddressMap<Account>,
        storage: &AddressMap<StorageOverlay>,
    ) {
        let index = self.bal_index;
        let Some(bal) = self.bal_builder.as_mut() else {
            return;
        };
        for (&address, entry) in accounts {
            bal.update_account(index, address, entry.original.as_ref(), entry.present.as_ref());
        }
        for (&address, overlay) in storage {
            bal.accounts.entry(address).or_default().storage.update_pending(index, &overlay.slots);
        }
    }

    /// Takes the built BAL, resetting the block access index. Returns `None` when BAL construction
    /// is disabled.
    #[inline]
    pub const fn take_bal_builder(&mut self) -> Option<Bal> {
        self.reset_bal_index();
        self.bal_builder.take()
    }

    /// Takes the built BAL as a canonical EIP-7928 [`BlockAccessList`], resetting the block access
    /// index. Returns `None` when BAL construction is disabled.
    #[inline]
    pub fn take_alloy_bal(&mut self) -> Option<BlockAccessList> {
        self.take_bal_builder().map(BlockAccessList::from)
    }

    /// Resolves `address` in the attached read BAL.
    ///
    /// Returns `Ok(None)` when no BAL is attached, or when the account is uncovered but
    /// [`Self::set_allow_db_fallback`] is enabled. Returns [`BalError::AccountNotFound`] when the
    /// account is uncovered and fallback is disabled.
    #[inline]
    pub fn get_bal_account(&self, address: &Address) -> BalResult<Option<&AccountBal>> {
        let Some(bal) = &self.bal else {
            return Ok(None);
        };
        match bal.accounts.get(address) {
            Some(bal_account) => Ok(Some(bal_account)),
            None if self.allow_db_fallback => Ok(None),
            None => Err(BalError::AccountNotFound { address: *address }),
        }
    }

    /// Applies a resolved BAL account's info writes at the current index to `account`.
    ///
    /// `bal_account` comes from [`Self::get_bal_account`], resolved before the raw account is
    /// read from the cache/database.
    #[inline]
    pub fn populate_bal_account(
        &self,
        bal_account: &AccountBal,
        account: &mut Option<AccountInfo>,
    ) {
        let was_present = account.is_some();
        let mut info = account.take().unwrap_or_default();
        let changed = bal_account.populate_account_info(self.bal_index, &mut info);
        // An account absent from the database with no BAL writes stays absent.
        if changed || was_present {
            *account = Some(info);
        }
    }

    /// Resolves storage slot `key` for `address` from the attached read BAL at the current
    /// index.
    ///
    /// Returns `Ok(Some(value))` when the BAL has a write for the slot at or before the current
    /// index. Returns `Ok(None)` when no BAL is attached, when the slot is covered but has no
    /// applicable write (caller should read the cache/database), or when the account/slot is
    /// uncovered but [`Self::set_allow_db_fallback`] is enabled. Returns an error when the account
    /// or slot is uncovered and fallback is disabled.
    #[inline]
    pub fn bal_storage(&self, address: &Address, key: &Word) -> BalResult<Option<Word>> {
        let Some(bal) = &self.bal else {
            return Ok(None);
        };
        let Some(bal_account) = bal.accounts.get(address) else {
            if self.allow_db_fallback {
                return Ok(None);
            }
            return Err(BalError::AccountNotFound { address: *address });
        };

        match bal_account.storage.get_bal_changes(address, *key) {
            Ok(changes) => Ok(changes.get(self.bal_index).copied()),
            Err(BalError::SlotNotFound { .. }) if self.allow_db_fallback => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Stashes a BAL lookup error for later retrieval through [`Self::take_error`] and returns the
    /// sentinel [`ErrorCode::BAL_NOT_COVERED`].
    #[inline]
    pub const fn store_error(&mut self, err: BalError) -> ErrorCode {
        self.bal_error = Some(err);
        ErrorCode::BAL_NOT_COVERED
    }

    /// Takes the stashed BAL lookup error, if a read left one.
    ///
    /// [`Self::take_error`] resolves through the database's error hook, which is
    /// reached only from inside execution. This exposes the same error to a caller
    /// holding the [`Evm`](crate::Evm) afterwards, so a refused read can be
    /// reported with the address or slot that was missing rather than the
    /// [`ErrorCode::BAL_NOT_COVERED`] sentinel alone.
    #[inline]
    pub const fn take_bal_error(&mut self) -> Option<BalError> {
        self.bal_error.take()
    }

    /// Takes the stashed BAL error as an [`AnyError`] when `code` is
    /// [`ErrorCode::BAL_NOT_COVERED`].
    ///
    /// Returns `None` for any other code so the caller can fall back to the wrapped database's
    /// error resolution.
    #[inline]
    pub fn take_error(&mut self, code: ErrorCode) -> Option<AnyError> {
        if code != ErrorCode::BAL_NOT_COVERED {
            return None;
        }
        self.bal_error.take().map(AnyError::new)
    }
}

/// Folds streamed state changes into the BAL builder at the current [`BalContext::bal_index`].
///
/// This is the sink shape of the [`Self::commit_pending`] fold: changed accounts and storage
/// slots are
/// recorded as writes, loaded-but-unchanged ones -- the read callbacks -- as reads. The bytecode
/// and storage-wipe callbacks need no BAL action: code changes surface through
/// [`StateChangeSink::account`], and a wiped account's storage surfaces through the storage
/// callbacks. Every callback is a no-op when BAL construction is disabled.
impl StateChangeSink for BalContext {
    type Error = Infallible;

    #[inline]
    fn account(&mut self, change: AccountChangeRef<'_>) -> Result<(), Self::Error> {
        let index = self.bal_index;
        if let Some(bal) = self.bal_builder.as_mut() {
            let original = change.original.cloned().unwrap_or_default();
            let current = change.current.cloned().unwrap_or_default();
            bal.accounts
                .entry(change.address)
                .or_default()
                .account_info
                .update(index, &original, &current);
        }
        Ok(())
    }

    #[inline]
    fn storage(&mut self, change: StorageChange) -> Result<(), Self::Error> {
        let index = self.bal_index;
        if let Some(bal) = self.bal_builder.as_mut() {
            bal.accounts
                .entry(change.address)
                .or_default()
                .storage
                .storage
                .entry(change.key)
                .or_default()
                .update(index, &change.original, change.current);
        }
        Ok(())
    }

    #[inline]
    fn account_read(
        &mut self,
        address: Address,
        _info: Option<&AccountInfo>,
    ) -> Result<(), Self::Error> {
        if let Some(bal) = self.bal_builder.as_mut() {
            bal.accounts.entry(address).or_default();
        }
        Ok(())
    }

    #[inline]
    fn storage_read(
        &mut self,
        address: Address,
        key: Word,
        _value: Word,
    ) -> Result<(), Self::Error> {
        if let Some(bal) = self.bal_builder.as_mut() {
            bal.accounts.entry(address).or_default().storage.storage.entry(key).or_default();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;

    const ADDRESS: Address = Address::repeat_byte(0xab);

    #[test]
    fn clear_bal_restores_database_reads() {
        let mut context = BalContext::new().with_bal(Arc::new(Bal::new()));

        // Attached and empty, so an uncovered account is an error.
        assert!(context.get_bal_account(&ADDRESS).is_err());

        context.clear_bal();

        // Detached, so the read resolves from the cache/database instead.
        assert_eq!(context.get_bal_account(&ADDRESS), Ok(None));
        assert!(context.bal().is_none());
    }

    #[test]
    fn take_bal_error_returns_the_stashed_lookup_failure() {
        let mut context = BalContext::new().with_bal(Arc::new(Bal::new()));

        let err = context.get_bal_account(&ADDRESS).unwrap_err();
        let code = context.store_error(err);
        assert_eq!(code, ErrorCode::BAL_NOT_COVERED);

        // Names the address the refused read wanted, which the sentinel does not.
        assert_eq!(context.take_bal_error(), Some(BalError::AccountNotFound { address: ADDRESS }));
        // Taken once, so a later read does not see a stale failure.
        assert_eq!(context.take_bal_error(), None);
    }
}
