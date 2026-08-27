//! Validity-predicate evaluation and indexing for payload transactions.

use alloy_primitives::{
    Address, TxHash, U256,
    map::{HashMap, HashSet},
};
use base_execution_txpool::{PredicateContext, ValidityPredicate};
use revm::{Database, state::EvmState};

/// Location that currently blocks a parked validity predicate.
///
/// State keys ([`Self::Balance`], [`Self::Storage`]) are woken by
/// [`ParkedPredicateIndex::affected_by_state`]. Context keys
/// ([`Self::BlockNumber`], [`Self::FlashblockIndex`]) stay parked until the
/// iterator is rebuilt with an updated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidityPredicateKey {
    /// Account balance.
    Balance(Address),
    /// Contract storage slot.
    Storage(Address, U256),
    /// Number of the block currently being built.
    BlockNumber,
    /// Index of the flashblock currently being built.
    FlashblockIndex,
}

impl ValidityPredicateKey {
    /// Returns the location read by a predicate.
    pub const fn for_predicate(predicate: &ValidityPredicate) -> Self {
        match predicate {
            ValidityPredicate::Balance { address, .. } => Self::Balance(*address),
            ValidityPredicate::Storage { address, slot, .. } => Self::Storage(*address, *slot),
            ValidityPredicate::BlockNumber { .. } => Self::BlockNumber,
            ValidityPredicate::FlashblockIndex { .. } => Self::FlashblockIndex,
        }
    }

    /// Returns the first predicate that does not hold against `db` and `context`.
    ///
    /// `Ok(None)` means every predicate matches. `Err` means a predicate's state could not be
    /// read; callers must treat that as an inability to verify rather than a successful match.
    pub fn first_unsatisfied<DB: Database>(
        predicates: &[ValidityPredicate],
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<Option<Self>, DB::Error> {
        for predicate in predicates {
            match predicate.matches(db, context) {
                Ok(true) => {}
                Ok(false) => return Ok(Some(Self::for_predicate(predicate))),
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

/// Result of evaluating a transaction's validity predicates at one build position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidityPredicateEvaluation {
    /// Every predicate is satisfied.
    Matched,
    /// The transaction is blocked by its first unsatisfied predicate.
    Unsatisfied {
        /// State or position key that currently blocks the transaction.
        blocker: ValidityPredicateKey,
        /// Whether the predicate batch can never be satisfied at a later build position.
        expired: bool,
    },
}

impl ValidityPredicateEvaluation {
    /// Evaluates predicates against the current state and build position.
    ///
    /// Returns a database error when a state-reading predicate cannot be verified.
    pub fn evaluate<DB: Database>(
        predicates: &[ValidityPredicate],
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<Self, DB::Error> {
        let Some(blocker) = ValidityPredicateKey::first_unsatisfied(predicates, db, context)?
        else {
            return Ok(Self::Matched);
        };
        Ok(Self::Unsatisfied {
            blocker,
            expired: ValidityPredicate::is_batch_expired(predicates, context),
        })
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

    /// Returns parked transactions and index-bucket wakeups triggered by `state`.
    pub fn affected_by_state(&self, state: &EvmState) -> StateChangeEffects {
        let mut effects = StateChangeEffects::default();
        for (address, account) in state {
            if account.info.balance != account.original_info().balance
                && let Some(hashes) = self.blockers.get(&ValidityPredicateKey::Balance(*address))
            {
                effects.affected_transactions.extend(hashes.iter().copied());
                effects.woken_buckets += 1;
            }

            // Selfdestruct can clear slots that were not loaded during this execution. Wake every
            // storage blocker for the account so those predicates are re-read from committed state.
            if account.is_selfdestructed() {
                for (key, hashes) in &self.blockers {
                    if matches!(key, ValidityPredicateKey::Storage(key_address, _) if key_address == address)
                    {
                        effects.affected_transactions.extend(hashes.iter().copied());
                        effects.woken_buckets += 1;
                    }
                }
            } else {
                for (slot, value) in &account.storage {
                    if value.is_changed()
                        && let Some(hashes) =
                            self.blockers.get(&ValidityPredicateKey::Storage(*address, *slot))
                    {
                        effects.affected_transactions.extend(hashes.iter().copied());
                        effects.woken_buckets += 1;
                    }
                }
            }
        }
        effects
    }

    /// Returns the number of parked transactions blocked on each distinct index bucket.
    pub fn bucket_depths(&self) -> impl Iterator<Item = usize> + '_ {
        self.blockers.values().map(HashSet::len)
    }
}

/// Parked transactions and index-bucket wakeups triggered by one state change.
///
/// A bucket is "woken" when the watched [`ValidityPredicateKey`] it indexes actually changed,
/// counted once per bucket regardless of how many parked transactions block on it. A wakeup
/// means the bucket's parked transactions are due for re-evaluation — it does not mean their
/// predicates became satisfied; that outcome is determined separately by the rescan.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StateChangeEffects {
    /// Parked transactions whose blocking predicate may have changed and need re-evaluation.
    pub affected_transactions: Vec<TxHash>,
    /// Number of distinct index buckets that were woken.
    pub woken_buckets: usize,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, map::HashSet};
    use base_execution_txpool::{PredicateContext, ValidityOperator, ValidityPredicate};
    use revm::{
        database::InMemoryDB,
        state::{Account, EvmState, EvmStorageSlot},
    };

    use super::{ParkedPredicateIndex, StateChangeEffects, ValidityPredicateKey};

    fn balance_predicate(address: Address, value: U256) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op: ValidityOperator::Equal, value }
    }

    fn test_context() -> PredicateContext {
        PredicateContext { block_number: 0, flashblock_index: 0 }
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

        assert_eq!(
            index.affected_by_state(&state),
            StateChangeEffects { affected_transactions: vec![balance_hash], woken_buckets: 1 }
        );

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

        assert_eq!(
            index.affected_by_state(&state),
            StateChangeEffects { affected_transactions: vec![balance_hash], woken_buckets: 1 }
        );
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

        assert_eq!(
            index.affected_by_state(&state),
            StateChangeEffects { affected_transactions: vec![hash], woken_buckets: 1 }
        );
    }

    #[test]
    fn selfdestruct_wakes_every_matching_storage_bucket() {
        let address = Address::with_last_byte(1);
        let first_hash = B256::with_last_byte(1);
        let second_hash = B256::with_last_byte(2);
        let mut index = ParkedPredicateIndex::default();
        index.park(first_hash, (), ValidityPredicateKey::Storage(address, U256::from(7)));
        index.park(second_hash, (), ValidityPredicateKey::Storage(address, U256::from(8)));

        let mut account = Account::default();
        account.mark_selfdestruct();
        let mut state = EvmState::default();
        state.insert(address, account);

        let effects = index.affected_by_state(&state);
        assert_eq!(effects.woken_buckets, 2);
        assert_eq!(
            effects.affected_transactions.into_iter().collect::<HashSet<_>>(),
            HashSet::from_iter([first_hash, second_hash])
        );
    }

    #[test]
    fn distinct_accounts_each_wake_their_own_bucket() {
        let balance_address = Address::with_last_byte(1);
        let storage_address = Address::with_last_byte(2);
        let balance_hash = B256::with_last_byte(1);
        let storage_hash = B256::with_last_byte(2);
        let mut index = ParkedPredicateIndex::default();
        index.park(balance_hash, (), ValidityPredicateKey::Balance(balance_address));
        index.park(storage_hash, (), ValidityPredicateKey::Storage(storage_address, U256::from(7)));

        let mut state = EvmState::default();
        let mut changed_balance = Account::default();
        changed_balance.info.balance = U256::ONE;
        state.insert(balance_address, changed_balance);
        let mut changed_storage = Account::default();
        changed_storage.storage.insert(
            U256::from(7),
            EvmStorageSlot::new_changed(U256::ZERO, U256::ONE, Default::default()),
        );
        state.insert(storage_address, changed_storage);

        let effects = index.affected_by_state(&state);
        assert_eq!(effects.woken_buckets, 2);
        assert_eq!(
            effects.affected_transactions.into_iter().collect::<HashSet<_>>(),
            HashSet::from_iter([balance_hash, storage_hash])
        );
    }

    #[test]
    fn shared_bucket_wakes_once_but_affects_every_rider() {
        let shared_address = Address::with_last_byte(1);
        let first_hash = B256::with_last_byte(1);
        let second_hash = B256::with_last_byte(2);
        let mut index = ParkedPredicateIndex::default();
        index.park(first_hash, (), ValidityPredicateKey::Balance(shared_address));
        index.park(second_hash, (), ValidityPredicateKey::Balance(shared_address));

        let mut state = EvmState::default();
        let mut account = Account::default();
        account.info.balance = U256::ONE;
        state.insert(shared_address, account);

        let effects = index.affected_by_state(&state);
        assert_eq!(effects.woken_buckets, 1);
        assert_eq!(
            effects.affected_transactions.into_iter().collect::<HashSet<_>>(),
            HashSet::from_iter([first_hash, second_hash])
        );
    }

    #[test]
    fn bucket_depths_reflects_parked_transactions_per_bucket() {
        let shared_address = Address::with_last_byte(1);
        let unique_address = Address::with_last_byte(2);
        let mut index = ParkedPredicateIndex::default();
        index.park(B256::with_last_byte(1), (), ValidityPredicateKey::Balance(shared_address));
        index.park(B256::with_last_byte(2), (), ValidityPredicateKey::Balance(shared_address));
        index.park(B256::with_last_byte(3), (), ValidityPredicateKey::Balance(unique_address));

        let mut depths: Vec<usize> = index.bucket_depths().collect();
        depths.sort_unstable();
        assert_eq!(depths, vec![1, 2]);
    }

    #[test]
    fn first_unsatisfied_returns_first_failing_predicate() {
        let mut db = InMemoryDB::default();
        let passing = balance_predicate(Address::with_last_byte(1), U256::ZERO);
        let failing = balance_predicate(Address::with_last_byte(2), U256::ONE);

        let predicates = [passing, failing];
        let context = test_context();
        assert_eq!(ValidityPredicateKey::first_unsatisfied(&[], &mut db, &context).unwrap(), None);
        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&predicates[..1], &mut db, &context).unwrap(),
            None
        );
        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&predicates, &mut db, &context).unwrap(),
            Some(ValidityPredicateKey::Balance(Address::with_last_byte(2)))
        );
    }

    #[test]
    fn evaluation_classifies_expired_position_predicates() {
        let mut db = InMemoryDB::default();
        let context = PredicateContext { block_number: 2, flashblock_index: 1 };
        let predicates =
            [ValidityPredicate::BlockNumber { op: ValidityOperator::Equal, value: U256::from(1) }];

        assert_eq!(
            super::ValidityPredicateEvaluation::evaluate(&predicates, &mut db, &context).unwrap(),
            super::ValidityPredicateEvaluation::Unsatisfied {
                blocker: ValidityPredicateKey::BlockNumber,
                expired: true,
            }
        );
    }

    #[test]
    fn first_unsatisfied_indexes_context_predicates() {
        let mut db = InMemoryDB::default();
        let context = PredicateContext { block_number: 1, flashblock_index: 0 };
        let block_number =
            ValidityPredicate::BlockNumber { op: ValidityOperator::Equal, value: U256::from(2) };
        let flashblock_index = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::Equal,
            value: U256::from(1),
        };

        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&[block_number], &mut db, &context).unwrap(),
            Some(ValidityPredicateKey::BlockNumber)
        );
        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&[flashblock_index], &mut db, &context)
                .unwrap(),
            Some(ValidityPredicateKey::FlashblockIndex)
        );

        let passing =
            ValidityPredicate::BlockNumber { op: ValidityOperator::Equal, value: U256::from(1) };
        assert_eq!(
            ValidityPredicateKey::first_unsatisfied(&[passing], &mut db, &context).unwrap(),
            None
        );
    }

    #[test]
    fn context_blockers_are_not_woken_by_state() {
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::default();
        index.park(hash, (), ValidityPredicateKey::BlockNumber);

        let mut state = EvmState::default();
        let mut account = Account::default();
        account.info.balance = U256::ONE;
        state.insert(Address::with_last_byte(1), account);

        assert_eq!(index.affected_by_state(&state), StateChangeEffects::default());
    }
}
