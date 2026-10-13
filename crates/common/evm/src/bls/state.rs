//! Read-only state access used to validate native BLS transactions.

use alloy_primitives::{Address, B256, U256};
use revm::{Database, context_interface::JournalTr, state::AccountInfo};

/// Read-only state access needed to validate a native BLS transaction.
///
/// Implemented over a plain [`Database`] ([`BlsDatabaseState`], used by transaction-pool
/// admission against the latest state) and over the transaction journal ([`BlsJournalState`],
/// used by execution so validation sees the same pending in-block state the transition mutates).
pub trait BlsState {
    /// State access error.
    type Error;

    /// Returns the account at `address`, or `None` if it does not exist.
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error>;

    /// Returns the storage word at `slot` of the account at `address`.
    fn storage(&mut self, address: Address, slot: B256) -> Result<U256, Self::Error>;
}

/// [`BlsState`] over a revm [`Database`].
#[derive(Debug)]
pub struct BlsDatabaseState<'a, DB>(pub &'a mut DB);

impl<DB: Database> BlsState for BlsDatabaseState<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic(address)
    }

    fn storage(&mut self, address: Address, slot: B256) -> Result<U256, Self::Error> {
        self.0.storage(address, slot.into())
    }
}

/// [`BlsState`] over a transaction journal. Reads warm the accessed accounts and slots but never
/// modify them.
#[derive(Debug)]
pub struct BlsJournalState<'a, J>(pub &'a mut J);

impl<J: JournalTr> BlsState for BlsJournalState<'_, J> {
    type Error = <J::Database as Database>::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // A missing account loads as the default (empty) info, which validation treats exactly
        // like `None`.
        Ok(Some(self.0.load_account(address)?.data.info.clone()))
    }

    fn storage(&mut self, address: Address, slot: B256) -> Result<U256, Self::Error> {
        // The journal reads storage only of loaded accounts.
        self.0.load_account(address)?;
        Ok(self.0.sload(address, slot.into())?.data)
    }
}
