//! State-key index for predicate-parked payload transactions.

use alloy_primitives::{
    Address, TxHash, U256,
    map::{HashMap, HashSet},
};
use base_execution_txpool::ValidityPredicate;
use revm::state::EvmState;

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

    /// Iterates predicates circularly from an offset derived from the transaction hash.
    ///
    /// This deterministically spreads parked transactions across their unsatisfied state keys when
    /// predicate lists share the same ordering.
    pub fn hash_rotated_scan(
        predicates: &[ValidityPredicate],
        transaction_hash: TxHash,
    ) -> impl Iterator<Item = &ValidityPredicate> {
        let start = if predicates.is_empty() {
            0
        } else {
            let hash = transaction_hash.as_slice();
            u64::from_be_bytes([
                hash[24], hash[25], hash[26], hash[27], hash[28], hash[29], hash[30], hash[31],
            ]) as usize
                % predicates.len()
        };
        predicates[start..].iter().chain(predicates[..start].iter())
    }

    /// Applies a function to predicates in the configured blocker-selection order until it returns
    /// a value.
    pub fn find_map_in_scan_order<'a, T>(
        predicates: &'a [ValidityPredicate],
        transaction_hash: TxHash,
        hash_rotated: bool,
        function: impl FnMut(&'a ValidityPredicate) -> Option<T>,
    ) -> Option<T> {
        if hash_rotated {
            Self::hash_rotated_scan(predicates, transaction_hash).find_map(function)
        } else {
            predicates.iter().find_map(function)
        }
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

    /// Returns the number of parked transactions.
    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    /// Returns the number of state keys currently blocking parked transactions.
    pub fn blocker_key_count(&self) -> usize {
        self.blockers.len()
    }

    /// Returns the largest current blocker bucket size.
    pub fn largest_blocker_bucket_size(&self) -> usize {
        self.blockers.values().map(HashSet::len).max().unwrap_or_default()
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
    use revm::state::{Account, EvmState, EvmStorageSlot};

    use super::{ParkedPredicateIndex, ValidityPredicateKey};

    fn balance_predicate(index: u8) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address: Address::with_last_byte(index),
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        }
    }

    #[test]
    fn hash_rotated_scan_is_deterministic_and_wraps() {
        let predicates = [balance_predicate(0), balance_predicate(1), balance_predicate(2)];
        let transaction_hash = B256::with_last_byte(2);

        let scan = || {
            ValidityPredicateKey::hash_rotated_scan(&predicates, transaction_hash)
                .map(ValidityPredicateKey::for_predicate)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            scan(),
            vec![
                ValidityPredicateKey::Balance(Address::with_last_byte(2)),
                ValidityPredicateKey::Balance(Address::with_last_byte(0)),
                ValidityPredicateKey::Balance(Address::with_last_byte(1)),
            ]
        );
        assert_eq!(scan(), scan());
    }

    #[test]
    fn hash_rotated_scan_distributes_starting_predicates() {
        let predicates = [balance_predicate(0), balance_predicate(1), balance_predicate(2)];

        let starts = (0..3)
            .map(|index| {
                ValidityPredicateKey::hash_rotated_scan(&predicates, B256::with_last_byte(index))
                    .next()
                    .map(ValidityPredicateKey::for_predicate)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            starts,
            vec![
                Some(ValidityPredicateKey::Balance(Address::with_last_byte(0))),
                Some(ValidityPredicateKey::Balance(Address::with_last_byte(1))),
                Some(ValidityPredicateKey::Balance(Address::with_last_byte(2))),
            ]
        );
        assert_eq!(ValidityPredicateKey::hash_rotated_scan(&[], B256::ZERO).next(), None);
    }

    #[test]
    fn scan_preserves_original_order_when_hash_rotation_is_disabled() {
        let predicates = [balance_predicate(0), balance_predicate(1), balance_predicate(2)];
        let mut keys = Vec::new();
        ValidityPredicateKey::find_map_in_scan_order(
            &predicates,
            B256::with_last_byte(2),
            false,
            |predicate| {
                keys.push(ValidityPredicateKey::for_predicate(predicate));
                None::<()>
            },
        );

        assert_eq!(
            keys,
            vec![
                ValidityPredicateKey::Balance(Address::with_last_byte(0)),
                ValidityPredicateKey::Balance(Address::with_last_byte(1)),
                ValidityPredicateKey::Balance(Address::with_last_byte(2)),
            ]
        );
    }

    #[test]
    fn reports_current_blocker_topology() {
        let mut index = ParkedPredicateIndex::default();
        let shared = ValidityPredicateKey::Balance(Address::ZERO);
        index.park(B256::with_last_byte(1), (), shared);
        index.park(B256::with_last_byte(2), (), shared);
        index.park(
            B256::with_last_byte(3),
            (),
            ValidityPredicateKey::Balance(Address::with_last_byte(1)),
        );

        assert_eq!(index.len(), 3);
        assert_eq!(index.blocker_key_count(), 2);
        assert_eq!(index.largest_blocker_bucket_size(), 2);
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

        assert_eq!(index.len(), 2);
        assert_eq!(index.blocker_key_count(), 2);
        assert_eq!(index.largest_blocker_bucket_size(), 1);

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
        assert_eq!(index.len(), 1);
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
}
