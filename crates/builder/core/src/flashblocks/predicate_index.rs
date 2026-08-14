//! State-key index for predicate-parked payload transactions.

use alloy_primitives::{
    Address, TxHash, U256,
    map::{HashMap, HashSet},
};
use base_execution_txpool::ValidityPredicate;
use revm::{Database, state::EvmState};

/// State location read by a validity predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidityPredicateKey {
    /// Account balance.
    Balance(Address),
    /// Contract storage slot.
    Storage(Address, U256),
}

impl ValidityPredicateKey {
    /// Returns the state location read by a predicate.
    pub const fn for_predicate(predicate: &ValidityPredicate) -> Self {
        match predicate {
            ValidityPredicate::Balance { address, .. } => Self::Balance(*address),
            ValidityPredicate::Storage { address, slot, .. } => Self::Storage(*address, *slot),
        }
    }

    /// Returns the first predicate that does not hold against `db`.
    ///
    /// `Ok(None)` means every predicate matches. `Err` means a predicate's state could not be
    /// read; callers must treat that as an inability to verify rather than a successful match.
    pub fn first_unsatisfied<DB: Database>(
        predicates: &[ValidityPredicate],
        db: &mut DB,
    ) -> Result<Option<Self>, DB::Error> {
        for predicate in predicates {
            match predicate.matches_state(db) {
                Ok(true) => {}
                Ok(false) => return Ok(Some(Self::for_predicate(predicate))),
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

/// Predicate-parked transactions indexed by one currently unsatisfied state location.
///
/// A parked transaction only needs one blocker in the index. When that location changes, callers
/// re-evaluate all of the transaction's predicates and either promote it or replace its blocker.
#[derive(Debug)]
pub struct ParkedPredicateIndex<T> {
    blockers: HashMap<ValidityPredicateKey, HashSet<TxHash>>,
    transactions: HashMap<TxHash, (T, ValidityPredicateKey)>,
}

impl<T> Default for ParkedPredicateIndex<T> {
    fn default() -> Self {
        Self { blockers: HashMap::default(), transactions: HashMap::default() }
    }
}

impl<T> ParkedPredicateIndex<T> {
    /// Returns whether no transactions are indexed.
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    /// Adds a parked transaction under one currently unsatisfied predicate key.
    pub fn park(
        &mut self,
        transaction_hash: TxHash,
        transaction: T,
        blocker: ValidityPredicateKey,
    ) {
        self.remove(transaction_hash);
        self.transactions.insert(transaction_hash, (transaction, blocker));
        self.blockers.entry(blocker).or_default().insert(transaction_hash);
    }

    /// Returns an indexed parked transaction.
    pub fn transaction(&self, transaction_hash: TxHash) -> Option<&T> {
        self.transactions.get(&transaction_hash).map(|(transaction, _)| transaction)
    }

    /// Replaces a parked transaction's currently unsatisfied predicate key.
    pub fn reindex(&mut self, transaction_hash: TxHash, blocker: ValidityPredicateKey) -> bool {
        let Some((_, previous)) = self.transactions.get_mut(&transaction_hash) else {
            return false;
        };
        if *previous == blocker {
            return true;
        }

        let previous = core::mem::replace(previous, blocker);
        if let Some(hashes) = self.blockers.get_mut(&previous) {
            hashes.remove(&transaction_hash);
            if hashes.is_empty() {
                self.blockers.remove(&previous);
            }
        }
        self.blockers.entry(blocker).or_default().insert(transaction_hash);
        true
    }

    /// Removes and returns an indexed parked transaction.
    pub fn remove(&mut self, transaction_hash: TxHash) -> Option<T> {
        let (transaction, blocker) = self.transactions.remove(&transaction_hash)?;
        if let Some(hashes) = self.blockers.get_mut(&blocker) {
            hashes.remove(&transaction_hash);
            if hashes.is_empty() {
                self.blockers.remove(&blocker);
            }
        }
        Some(transaction)
    }

    /// Returns parked transactions whose indexed state may have changed.
    pub fn affected_by_state(&self, state: &EvmState) -> Vec<TxHash> {
        let mut affected = Vec::new();
        for (address, account) in state {
            if account.info.balance != account.original_info().balance
                && let Some(hashes) = self.blockers.get(&ValidityPredicateKey::Balance(*address))
            {
                affected.extend(hashes.iter().copied());
            }

            // Selfdestruct can clear slots that were not loaded during this execution. Wake every
            // storage blocker for the account so those predicates are re-read from committed state.
            if account.is_selfdestructed() {
                for (key, hashes) in &self.blockers {
                    if matches!(key, ValidityPredicateKey::Storage(key_address, _) if key_address == address)
                    {
                        affected.extend(hashes.iter().copied());
                    }
                }
            } else {
                for (slot, value) in &account.storage {
                    if value.is_changed()
                        && let Some(hashes) =
                            self.blockers.get(&ValidityPredicateKey::Storage(*address, *slot))
                    {
                        affected.extend(hashes.iter().copied());
                    }
                }
            }
        }
        affected
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use base_execution_txpool::{ValidityOperator, ValidityPredicate};
    use revm::{
        database::InMemoryDB,
        state::{Account, EvmState, EvmStorageSlot},
    };

    use super::{ParkedPredicateIndex, ValidityPredicateKey};

    fn balance_predicate(address: Address, value: U256) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op: ValidityOperator::Equal, value }
    }

    #[test]
    fn indexes_only_transactions_affected_by_changed_state() {
        let balance_address = Address::with_last_byte(1);
        let storage_address = Address::with_last_byte(2);
        let balance_hash = B256::with_last_byte(1);
        let storage_hash = B256::with_last_byte(2);
        let mut index = ParkedPredicateIndex::default();
        index.park(balance_hash, 1, ValidityPredicateKey::Balance(balance_address));
        index.park(storage_hash, 2, ValidityPredicateKey::Storage(storage_address, U256::from(7)));

        let mut state = EvmState::default();
        let mut account = Account::default();
        account.info.balance = U256::ONE;
        state.insert(balance_address, account);

        assert_eq!(index.affected_by_state(&state), vec![balance_hash]);

        assert!(
            index.reindex(
                balance_hash,
                ValidityPredicateKey::Storage(storage_address, U256::from(8))
            )
        );
        let mut account = Account::default();
        account.storage.insert(
            U256::from(8),
            EvmStorageSlot::new_changed(U256::ZERO, U256::ONE, Default::default()),
        );
        state.clear();
        state.insert(storage_address, account);

        assert_eq!(index.affected_by_state(&state), vec![balance_hash]);
        assert_eq!(index.remove(balance_hash), Some(1));
        assert_eq!(index.transaction(storage_hash), Some(&2));
    }

    #[test]
    fn selfdestruct_wakes_unloaded_storage_blockers() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::default();
        index.park(hash, (), ValidityPredicateKey::Storage(address, U256::from(7)));

        let mut account = Account::default();
        account.mark_selfdestruct();
        let mut state = EvmState::default();
        state.insert(address, account);

        assert_eq!(index.affected_by_state(&state), vec![hash]);
    }

    #[test]
    fn first_unsatisfied_returns_first_failing_predicate() {
        let mut db = InMemoryDB::default();
        let passing = balance_predicate(Address::with_last_byte(1), U256::ZERO);
        let failing = balance_predicate(Address::with_last_byte(2), U256::ONE);

        let predicates = [passing, failing];
        assert_eq!(ValidityPredicateKey::first_unsatisfied(&[], &mut db).unwrap(), None);
        assert_eq!(ValidityPredicateKey::first_unsatisfied(&predicates[..1], &mut db).unwrap(), None);
        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&predicates, &mut db).unwrap(),
            Some(ValidityPredicateKey::Balance(Address::with_last_byte(2)))
        );
    }
}
