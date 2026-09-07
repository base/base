//! State database abstraction.

use crate::Database;
use revm::{
    database::State, database_interface::bal::BalDatabase, state::bal::BlockAccessIndex,
    DatabaseCommit,
};

/// Database that tracks the current block-level access list (BAL) index from EIP-7928.
///
/// BAL values are indexed by their position in block execution. Index `0` is reserved for
/// pre-transaction block execution changes, such as system contract calls. Regular transactions
/// start at index `1`, so transaction `0` in the block uses BAL index `1`, transaction `1` uses BAL
/// index `2`, and so on. Post-transaction block execution changes use the index after the last
/// transaction.
pub trait BalIndexedDatabase: Database {
    /// Sets the current EIP-7928 BAL index for subsequent database reads and writes.
    ///
    /// Use index `0` for pre-transaction block execution, `tx_index + 1` for regular transactions
    /// in the block, and the next index after the last transaction for post-transaction block
    /// execution. In other words, regular block transactions start at BAL index `1`.
    fn set_bal_index(&mut self, index: u64);

    /// Advances the current BAL index.
    fn bump_bal_index(&mut self);
}

impl<DB> BalIndexedDatabase for State<DB>
where
    Self: Database,
{
    fn set_bal_index(&mut self, index: u64) {
        self.bal_state.bal_index = BlockAccessIndex::new(index);
    }

    fn bump_bal_index(&mut self) {
        self.bal_state.bump_bal_index();
    }
}

impl<DB> BalIndexedDatabase for BalDatabase<DB>
where
    Self: Database,
{
    fn set_bal_index(&mut self, index: u64) {
        self.bal_state.bal_index = BlockAccessIndex::new(index);
    }

    fn bump_bal_index(&mut self) {
        self.bal_state.bump_bal_index();
    }
}

/// Alias trait for [`Database`] and [`DatabaseCommit`].
pub trait StateDB: Database + DatabaseCommit {}

impl<T> StateDB for T where T: Database + DatabaseCommit {}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{database::CacheDB, database_interface::EmptyDB};

    #[test]
    fn state_sets_and_bumps_bal_index() {
        let mut db = State::builder().with_database(CacheDB::new(EmptyDB::new())).build();

        BalIndexedDatabase::set_bal_index(&mut db, 7);
        assert_eq!(db.bal_state.bal_index, BlockAccessIndex::new(7));

        BalIndexedDatabase::bump_bal_index(&mut db);
        assert_eq!(db.bal_state.bal_index, BlockAccessIndex::new(8));
    }
}
