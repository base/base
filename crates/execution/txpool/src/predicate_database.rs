//! Database reads used by validity predicates.

use alloy_primitives::{Address, U256};
use revm::{
    Database, DatabaseRef,
    database::{CacheDB, State, bal::EvmDatabaseError},
};

/// Database operations used to evaluate validity predicates.
pub trait PredicateDatabase: Database {
    /// Reads an account balance without materializing unrelated account fields when supported.
    fn balance(&mut self, address: Address) -> Result<U256, Self::Error> {
        self.basic(address).map(|account| account.map_or(U256::ZERO, |account| account.balance))
    }
}

impl<ExtDB: DatabaseRef> PredicateDatabase for CacheDB<ExtDB> {}

impl<DB: Database> PredicateDatabase for State<DB> {
    fn balance(&mut self, address: Address) -> Result<U256, Self::Error> {
        if self.has_bal() {
            return self
                .basic(address)
                .map(|account| account.map_or(U256::ZERO, |account| account.balance));
        }
        self.load_cache_account(address)
            .map(|account| {
                account.account.as_ref().map_or(U256::ZERO, |account| account.info.balance)
            })
            .map_err(EvmDatabaseError::Database)
    }
}

impl<DB: PredicateDatabase + ?Sized> PredicateDatabase for &mut DB {
    fn balance(&mut self, address: Address) -> Result<U256, Self::Error> {
        (**self).balance(address)
    }
}
