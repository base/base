//! Validity-predicate evaluation and indexing for payload transactions.

use core::ops::Bound::{Excluded, Included};
use std::collections::BTreeMap;

use alloy_primitives::{
    Address, TxHash, U256,
    map::{B256Map, B256Set, HashMap, U256Map},
};
use base_execution_txpool::{
    PredicateContext, ValidityConditions, ValidityOperator, ValidityPredicate,
};
use revm::{Database, state::EvmState};

/// The number of parked predicates at which a flat bucket becomes ordered by default.
pub const DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD: usize = 32;

/// Location that currently blocks a parked validity predicate.
///
/// State keys ([`Self::Balance`], [`Self::Nonce`], [`Self::Storage`]) are woken by
/// [`ParkedPredicateIndex::affected_by_state`]. Context keys
/// ([`Self::BlockNumber`], [`Self::FlashblockIndex`]) stay parked until the
/// iterator is rebuilt with an updated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidityPredicateKey {
    /// Account balance.
    Balance(Address),
    /// Account protocol nonce.
    Nonce(Address),
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
            ValidityPredicate::Nonce { address, .. } => Self::Nonce(*address),
            ValidityPredicate::Storage { address, slot, .. } => Self::Storage(*address, *slot),
            ValidityPredicate::BlockNumber { .. } => Self::BlockNumber,
            ValidityPredicate::FlashblockIndex { .. } => Self::FlashblockIndex,
        }
    }

    /// Returns the first unsatisfied normalized predicate and its state or context key.
    ///
    /// State predicates are borrowed during evaluation and cloned only when unsatisfied.
    pub fn first_unsatisfied<DB: Database>(
        predicates: &ValidityConditions,
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<Option<(ValidityPredicate, Self)>, DB::Error> {
        if let Some((op, value)) =
            predicates.block().first_unsatisfied(U256::from(context.block_number))
        {
            return Ok(Some((ValidityPredicate::BlockNumber { op, value }, Self::BlockNumber)));
        }
        if let Some((op, value)) =
            predicates.flashblock().first_unsatisfied(U256::from(context.flashblock_index))
        {
            return Ok(Some((
                ValidityPredicate::FlashblockIndex { op, value },
                Self::FlashblockIndex,
            )));
        }
        for predicate in predicates.state_predicates() {
            if !predicate.matches(db, context)? {
                return Ok(Some((predicate.clone(), Self::for_predicate(predicate))));
            }
        }
        Ok(None)
    }
}

/// Result of evaluating a transaction's validity predicates at one build position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidityPredicateEvaluation {
    /// Every predicate is satisfied.
    Matched,
    /// The transaction is blocked by its first unsatisfied predicate.
    Unsatisfied {
        /// State or position key that currently blocks the transaction.
        blocker: ValidityPredicateKey,
        /// Failed condition, returned directly so parking does not rescan the batch.
        predicate: ValidityPredicate,
        /// Whether the predicate batch can never be satisfied at a later build position.
        expired: bool,
    },
}

impl ValidityPredicateEvaluation {
    /// Evaluates predicates against the current state and build position.
    pub fn evaluate<DB: Database>(
        predicates: &ValidityConditions,
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<Self, DB::Error> {
        let Some((predicate, blocker)) =
            ValidityPredicateKey::first_unsatisfied(predicates, db, context)?
        else {
            return Ok(Self::Matched);
        };
        Ok(Self::Unsatisfied { blocker, predicate, expired: predicates.is_expired(context) })
    }
}

/// Predicate-parked transactions indexed by one currently unsatisfied state location.
///
/// Buckets start as compact hash sets. Once a state bucket reaches `ordered_threshold`, it is
/// converted for the rest of the build into an ordered representation. The latter wakes only
/// false predicates whose comparison can become true for the observed old/new state transition.
#[derive(Debug)]
pub struct ParkedPredicateIndex<T> {
    blockers: HashMap<ValidityPredicateKey, PredicateBucket>,
    transactions: B256Map<ParkedTransaction<T>>,
    ordered_threshold: usize,
}

#[derive(Debug)]
struct ParkedTransaction<T> {
    transaction: T,
    blocker: ValidityPredicateKey,
    predicate: ValidityPredicate,
}

#[derive(Debug)]
enum PredicateBucket {
    Flat(B256Set),
    Ordered(OrderedPredicateBucket),
}

#[derive(Debug, Default)]
struct OrderedPredicateBucket {
    thresholds: U256Map<BTreeMap<U256, B256Set>>,
    equal: U256Map<U256Map<B256Set>>,
    not_equal: U256Map<B256Set>,
    never: B256Set,
}

#[derive(Debug, Clone, Copy)]
enum OrderedPredicateKind {
    Threshold(U256),
    Equal(U256),
    NotEqual,
    Never,
}

impl<T> Default for ParkedPredicateIndex<T> {
    fn default() -> Self {
        Self::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD)
    }
}

impl<T> ParkedPredicateIndex<T> {
    /// Creates an index that converts state buckets at `ordered_threshold` entries.
    ///
    /// A zero threshold is treated as one, so every state bucket uses the ordered form.
    pub fn new(ordered_threshold: usize) -> Self {
        Self {
            blockers: HashMap::default(),
            transactions: B256Map::default(),
            ordered_threshold: ordered_threshold.max(1),
        }
    }

    /// Returns whether no transactions are indexed.
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    /// Adds a parked transaction under its currently unsatisfied predicate.
    pub fn park(&mut self, transaction_hash: TxHash, transaction: T, predicate: ValidityPredicate) {
        self.remove(transaction_hash);
        let blocker = ValidityPredicateKey::for_predicate(&predicate);
        self.transactions
            .insert(transaction_hash, ParkedTransaction { transaction, blocker, predicate });
        self.insert_into_bucket(transaction_hash);
    }

    /// Returns an indexed parked transaction.
    pub fn transaction(&self, transaction_hash: TxHash) -> Option<&T> {
        self.transactions.get(&transaction_hash).map(|entry| &entry.transaction)
    }

    /// Returns the predicate an indexed transaction is parked under.
    pub fn predicate(&self, transaction_hash: TxHash) -> Option<&ValidityPredicate> {
        self.transactions.get(&transaction_hash).map(|entry| &entry.predicate)
    }

    /// Replaces a parked transaction's currently unsatisfied predicate.
    pub fn reindex(&mut self, transaction_hash: TxHash, predicate: ValidityPredicate) -> bool {
        let Some(previous) = self.transactions.get(&transaction_hash) else {
            return false;
        };
        let old_blocker = previous.blocker;
        let old_predicate = previous.predicate.clone();
        self.remove_from_bucket(transaction_hash, old_blocker, &old_predicate);
        let blocker = ValidityPredicateKey::for_predicate(&predicate);
        let entry = self.transactions.get_mut(&transaction_hash).expect("entry was checked above");
        entry.blocker = blocker;
        entry.predicate = predicate;
        self.insert_into_bucket(transaction_hash);
        true
    }

    /// Removes and returns an indexed parked transaction.
    pub fn remove(&mut self, transaction_hash: TxHash) -> Option<T> {
        let entry = self.transactions.remove(&transaction_hash)?;
        self.remove_from_bucket(transaction_hash, entry.blocker, &entry.predicate);
        Some(entry.transaction)
    }

    /// Returns parked transactions and index-bucket wakeups triggered by `state`.
    pub fn affected_by_state(&self, state: &EvmState) -> StateChangeEffects {
        let mut effects = StateChangeEffects::default();
        for (address, account) in state {
            // Deleted accounts read as nonce zero, regardless of the retained account info.
            let new_nonce = if account.is_selfdestructed() { 0 } else { account.info.nonce };
            if new_nonce != account.original_info().nonce {
                self.wake_bucket(
                    &mut effects,
                    ValidityPredicateKey::Nonce(*address),
                    U256::from(account.original_info().nonce),
                    U256::from(new_nonce),
                );
            }
            if account.info.balance != account.original_info().balance {
                self.wake_bucket(
                    &mut effects,
                    ValidityPredicateKey::Balance(*address),
                    account.original_info().balance,
                    account.info.balance,
                );
            }
            if account.is_selfdestructed() {
                for (key, bucket) in &self.blockers {
                    if matches!(key, ValidityPredicateKey::Storage(key_address, _) if key_address == address)
                    {
                        bucket.extend_all(&mut effects.affected_transactions);
                        effects.woken_buckets += 1;
                    }
                }
            } else {
                for (slot, value) in &account.storage {
                    if value.is_changed() {
                        self.wake_bucket(
                            &mut effects,
                            ValidityPredicateKey::Storage(*address, *slot),
                            value.original_value(),
                            value.present_value(),
                        );
                    }
                }
            }
        }
        effects
    }

    /// Returns the number of parked transactions blocked on each distinct index bucket.
    pub fn bucket_depths(&self) -> impl Iterator<Item = usize> + '_ {
        self.blockers.values().map(PredicateBucket::len)
    }

    fn insert_into_bucket(&mut self, hash: TxHash) {
        let entry = self.transactions.get(&hash).expect("inserted transaction must exist");
        let blocker = entry.blocker;
        let predicate = &entry.predicate;
        let bucket = self
            .blockers
            .entry(blocker)
            .or_insert_with(|| PredicateBucket::Flat(B256Set::default()));
        bucket.insert(hash, predicate);
        if bucket.len() >= self.ordered_threshold && bucket.is_flat() && blocker.is_state() {
            let hashes = bucket.take_flat().expect("bucket was checked as flat");
            let mut ordered = OrderedPredicateBucket::default();
            for hash in hashes {
                let predicate =
                    &self.transactions.get(&hash).expect("bucket entry must exist").predicate;
                ordered.insert(hash, predicate);
            }
            *bucket = PredicateBucket::Ordered(ordered);
        }
    }

    fn remove_from_bucket(
        &mut self,
        hash: TxHash,
        blocker: ValidityPredicateKey,
        predicate: &ValidityPredicate,
    ) {
        let Some(bucket) = self.blockers.get_mut(&blocker) else { return };
        bucket.remove(hash, predicate);
        if bucket.is_empty() {
            self.blockers.remove(&blocker);
        }
    }

    fn wake_bucket(
        &self,
        effects: &mut StateChangeEffects,
        key: ValidityPredicateKey,
        old: U256,
        new: U256,
    ) {
        let Some(bucket) = self.blockers.get(&key) else { return };
        bucket.extend_affected(old, new, &mut effects.affected_transactions);
        effects.woken_buckets += 1;
    }
}

impl ValidityPredicateKey {
    const fn is_state(self) -> bool {
        matches!(self, Self::Balance(_) | Self::Nonce(_) | Self::Storage(_, _))
    }
}

impl PredicateBucket {
    fn insert(&mut self, hash: TxHash, predicate: &ValidityPredicate) {
        match self {
            Self::Flat(hashes) => {
                hashes.insert(hash);
            }
            Self::Ordered(bucket) => bucket.insert(hash, predicate),
        }
    }

    fn remove(&mut self, hash: TxHash, predicate: &ValidityPredicate) {
        match self {
            Self::Flat(hashes) => {
                hashes.remove(&hash);
            }
            Self::Ordered(bucket) => bucket.remove(hash, predicate),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Flat(hashes) => hashes.len(),
            Self::Ordered(bucket) => bucket.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    const fn is_flat(&self) -> bool {
        matches!(self, Self::Flat(_))
    }
    fn take_flat(&mut self) -> Option<B256Set> {
        match self {
            Self::Flat(hashes) => Some(core::mem::take(hashes)),
            Self::Ordered(_) => None,
        }
    }

    fn extend_all(&self, target: &mut Vec<TxHash>) {
        match self {
            Self::Flat(hashes) => target.extend(hashes.iter().copied()),
            Self::Ordered(bucket) => bucket.extend_all(target),
        }
    }

    fn extend_affected(&self, old: U256, new: U256, target: &mut Vec<TxHash>) {
        match self {
            Self::Flat(hashes) => target.extend(hashes.iter().copied()),
            Self::Ordered(bucket) => bucket.extend_affected(old, new, target),
        }
    }
}

impl OrderedPredicateBucket {
    fn insert(&mut self, hash: TxHash, predicate: &ValidityPredicate) {
        let Some((mask, kind)) = ordered_predicate(predicate) else { return };
        match kind {
            OrderedPredicateKind::Threshold(value) => {
                self.thresholds.entry(mask).or_default().entry(value).or_default().insert(hash);
            }
            OrderedPredicateKind::Equal(value) => {
                self.equal.entry(mask).or_default().entry(value).or_default().insert(hash);
            }
            OrderedPredicateKind::NotEqual => {
                self.not_equal.entry(mask).or_default().insert(hash);
            }
            OrderedPredicateKind::Never => {
                self.never.insert(hash);
            }
        }
    }

    fn remove(&mut self, hash: TxHash, predicate: &ValidityPredicate) {
        let Some((mask, kind)) = ordered_predicate(predicate) else { return };
        match kind {
            OrderedPredicateKind::Threshold(value) => {
                remove_hash(&mut self.thresholds, mask, Some(value), hash)
            }
            OrderedPredicateKind::Equal(value) => {
                remove_hash(&mut self.equal, mask, Some(value), hash)
            }
            OrderedPredicateKind::NotEqual => remove_hash(&mut self.not_equal, mask, None, hash),
            OrderedPredicateKind::Never => {
                self.never.remove(&hash);
            }
        }
    }

    fn len(&self) -> usize {
        self.thresholds.values().flat_map(BTreeMap::values).map(B256Set::len).sum::<usize>()
            + self.equal.values().flat_map(U256Map::values).map(B256Set::len).sum::<usize>()
            + self.not_equal.values().map(B256Set::len).sum::<usize>()
            + self.never.len()
    }

    fn extend_all(&self, target: &mut Vec<TxHash>) {
        target.extend(
            self.thresholds
                .values()
                .flat_map(BTreeMap::values)
                .chain(self.equal.values().flat_map(U256Map::values))
                .chain(self.not_equal.values())
                .chain(core::iter::once(&self.never))
                .flat_map(|hashes| hashes.iter().copied()),
        );
    }

    fn extend_affected(&self, old: U256, new: U256, target: &mut Vec<TxHash>) {
        for (mask, thresholds) in &self.thresholds {
            let old = old & *mask;
            let new = new & *mask;
            if old != new {
                let (lower, upper) = if old < new { (old, new) } else { (new, old) };
                for hashes in
                    thresholds.range((Excluded(lower), Included(upper))).map(|(_, hashes)| hashes)
                {
                    target.extend(hashes.iter().copied());
                }
            }
        }
        for (mask, equal) in &self.equal {
            if let Some(hashes) = equal.get(&(new & *mask)) {
                target.extend(hashes.iter().copied());
            }
        }
        for (mask, hashes) in &self.not_equal {
            if (old & *mask) != (new & *mask) {
                target.extend(hashes.iter().copied());
            }
        }
    }
}

fn ordered_predicate(predicate: &ValidityPredicate) -> Option<(U256, OrderedPredicateKind)> {
    let (mask, op, value) = match predicate {
        ValidityPredicate::Balance { op, value, .. }
        | ValidityPredicate::Nonce { op, value, .. } => (U256::MAX, *op, *value),
        ValidityPredicate::Storage { mask, op, value, .. } => (*mask, *op, *value),
        ValidityPredicate::BlockNumber { .. } | ValidityPredicate::FlashblockIndex { .. } => {
            return None;
        }
    };
    let kind = match op {
        ValidityOperator::LessThan | ValidityOperator::GreaterThanOrEqual => {
            OrderedPredicateKind::Threshold(value)
        }
        ValidityOperator::LessThanOrEqual | ValidityOperator::GreaterThan => value
            .checked_add(U256::ONE)
            .map_or(OrderedPredicateKind::Never, OrderedPredicateKind::Threshold),
        ValidityOperator::Equal => OrderedPredicateKind::Equal(value),
        ValidityOperator::NotEqual => OrderedPredicateKind::NotEqual,
    };
    Some((mask, kind))
}

fn remove_hash<M>(map: &mut U256Map<M>, mask: U256, value: Option<U256>, hash: TxHash)
where
    M: BucketMap,
{
    let remove_mask = map.get_mut(&mask).is_some_and(|bucket| bucket.remove_hash(value, hash));
    if remove_mask {
        map.remove(&mask);
    }
}

trait BucketMap {
    fn remove_hash(&mut self, value: Option<U256>, hash: TxHash) -> bool;
}

impl BucketMap for BTreeMap<U256, B256Set> {
    fn remove_hash(&mut self, value: Option<U256>, hash: TxHash) -> bool {
        let value = value.expect("threshold index has a value");
        if let Some(hashes) = self.get_mut(&value) {
            hashes.remove(&hash);
            if hashes.is_empty() {
                self.remove(&value);
            }
        }
        self.is_empty()
    }
}

impl BucketMap for U256Map<B256Set> {
    fn remove_hash(&mut self, value: Option<U256>, hash: TxHash) -> bool {
        let value = value.expect("point index has a value");
        if let Some(hashes) = self.get_mut(&value) {
            hashes.remove(&hash);
            if hashes.is_empty() {
                self.remove(&value);
            }
        }
        self.is_empty()
    }
}

impl BucketMap for B256Set {
    fn remove_hash(&mut self, _: Option<U256>, hash: TxHash) -> bool {
        self.remove(&hash);
        self.is_empty()
    }
}

/// Parked transactions and index-bucket wakeups triggered by one state change.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StateChangeEffects {
    /// Parked transactions whose blocking predicate may have changed and need re-evaluation.
    pub affected_transactions: Vec<TxHash>,
    /// Number of distinct index buckets that were woken.
    pub woken_buckets: usize,
}

#[cfg(test)]
pub mod test_utils {
    //! Hand-rolled evaluation database for a single ordered log spanning account and
    //! storage reads, including reads that fail. Separate method mocks cannot express
    //! this cross-method log.

    use alloy_primitives::{Address, B256, U256};
    use revm::{
        Database,
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };

    /// A scripted account or storage read failure.
    #[derive(Debug, PartialEq, Eq, thiserror::Error)]
    #[error("scripted state read failure")]
    pub struct ReadError;

    impl DBErrorMarker for ReadError {}

    /// In-memory state with ordered account/slot reads and optional error injection.
    #[derive(Debug, Default)]
    pub struct RecordingDatabase {
        /// State returned by successful reads.
        pub inner: InMemoryDB,
        /// Account reads have no slot; storage reads retain their slot.
        pub reads: Vec<(Address, Option<U256>)>,
        /// One-based read index at which to return an error.
        pub fail_on: Option<usize>,
    }

    impl Database for RecordingDatabase {
        type Error = ReadError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.reads.push((address, None));
            if self.fail_on == Some(self.reads.len()) {
                return Err(ReadError);
            }
            Ok(self.inner.basic(address).unwrap())
        }

        fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
            self.reads.push((address, Some(slot)));
            if self.fail_on == Some(self.reads.len()) {
                return Err(ReadError);
            }
            Ok(self.inner.storage(address, slot).unwrap())
        }

        fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
            panic!("validity predicates must not load bytecode");
        }

        fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
            panic!("validity predicates must not load block hashes");
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, map::B256Set};
    use base_execution_txpool::{
        PredicateContext, ValidityConditions, ValidityOperator, ValidityPredicate,
    };
    use revm::{
        database::InMemoryDB,
        state::{Account, AccountInfo, EvmState, EvmStorageSlot},
    };

    use super::{
        ParkedPredicateIndex, StateChangeEffects, ValidityPredicateEvaluation,
        ValidityPredicateKey,
        test_utils::{ReadError, RecordingDatabase},
    };

    fn balance(address: Address, op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op, value: U256::from(value) }
    }

    fn storage(
        address: Address,
        mask: U256,
        op: ValidityOperator,
        value: u64,
    ) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot: U256::from(7),
            mask,
            op,
            value: U256::from(value),
        }
    }

    fn changed_balance(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.info.balance = U256::from(new);
        account.original_info_mut().balance = U256::from(old);
        EvmState::from_iter([(address, account)])
    }

    fn changed_storage(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.storage.insert(
            U256::from(7),
            EvmStorageSlot::new_changed(U256::from(old), U256::from(new), Default::default()),
        );
        EvmState::from_iter([(address, account)])
    }

    #[test]
    fn state_partition_order_short_circuits_failures_and_propagates_errors() {
        let first = Address::with_last_byte(1);
        let second = Address::with_last_byte(2);
        let nonce_address = Address::with_last_byte(3);
        let storage_address = Address::with_last_byte(4);
        let ordered = [
            balance(first, ValidityOperator::Equal, 1),
            balance(second, ValidityOperator::Equal, 2),
            ValidityPredicate::Nonce {
                address: nonce_address,
                op: ValidityOperator::Equal,
                value: U256::from(3),
            },
            storage(storage_address, U256::from(0xff), ValidityOperator::Equal, 4),
        ];
        let conditions = ValidityConditions::new(vec![
            ordered[3].clone(),
            ordered[2].clone(),
            ordered[0].clone(),
            ordered[1].clone(),
        ]);
        let context = PredicateContext { block_number: 100, flashblock_index: 2 };
        let expected_reads = [
            (first, None),
            (second, None),
            (nonce_address, None),
            (storage_address, Some(U256::from(7))),
        ];
        let mut db = RecordingDatabase::default();
        for (index, predicate) in ordered.iter().enumerate() {
            db.reads.clear();
            let blocker = match index {
                0 => ValidityPredicateKey::Balance(first),
                1 => ValidityPredicateKey::Balance(second),
                2 => ValidityPredicateKey::Nonce(nonce_address),
                _ => ValidityPredicateKey::Storage(storage_address, U256::from(7)),
            };
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context),
                Ok(ValidityPredicateEvaluation::Unsatisfied {
                    blocker,
                    predicate: predicate.clone(),
                    expired: false
                })
            );
            assert!(conditions.contains(predicate));
            assert_eq!(db.reads, expected_reads[..=index]);
            match index {
                0 => db.inner.insert_account_info(
                    first,
                    AccountInfo { balance: U256::ONE, ..Default::default() },
                ),
                1 => db.inner.insert_account_info(
                    second,
                    AccountInfo { balance: U256::from(2), ..Default::default() },
                ),
                2 => db.inner.insert_account_info(
                    nonce_address,
                    AccountInfo { nonce: 3, ..Default::default() },
                ),
                _ => db
                    .inner
                    .insert_account_storage(storage_address, U256::from(7), U256::from(0x104))
                    .unwrap(),
            }
        }
        db.reads.clear();
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context),
            Ok(ValidityPredicateEvaluation::Matched)
        );
        assert_eq!(db.reads, expected_reads);
        for failing_read in 1..=4 {
            db.reads.clear();
            db.fail_on = Some(failing_read);
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context),
                Err(ReadError)
            );
            assert_eq!(db.reads, expected_reads[..failing_read]);
        }
    }

    #[test]
    fn timing_blockers_are_normalized_and_gate_even_failing_state_reads() {
        let state = balance(Address::ZERO, ValidityOperator::Equal, 1);
        let conditions = ValidityConditions::new(vec![
            state,
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::Equal,
                value: U256::from(2),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThan,
                value: U256::from(9),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThan,
                value: U256::from(21),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::NotEqual,
                value: U256::from(15),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::NotEqual,
                value: U256::from(15),
            },
        ]);
        for (block_number, flashblock_index, blocker, op, value, expired) in [
            (
                9,
                1,
                ValidityPredicateKey::BlockNumber,
                ValidityOperator::GreaterThanOrEqual,
                10,
                false,
            ),
            (15, 1, ValidityPredicateKey::BlockNumber, ValidityOperator::NotEqual, 15, false),
            (21, 1, ValidityPredicateKey::BlockNumber, ValidityOperator::LessThanOrEqual, 20, true),
            (
                10,
                1,
                ValidityPredicateKey::FlashblockIndex,
                ValidityOperator::GreaterThanOrEqual,
                2,
                false,
            ),
            (
                10,
                3,
                ValidityPredicateKey::FlashblockIndex,
                ValidityOperator::LessThanOrEqual,
                2,
                false,
            ),
            (
                20,
                3,
                ValidityPredicateKey::FlashblockIndex,
                ValidityOperator::LessThanOrEqual,
                2,
                true,
            ),
            // A later block resets the flashblock index, so a missed flashblock is not terminal.
            (
                11,
                1,
                ValidityPredicateKey::FlashblockIndex,
                ValidityOperator::GreaterThanOrEqual,
                2,
                false,
            ),
        ] {
            let context = PredicateContext { block_number, flashblock_index };
            let predicate = match blocker {
                ValidityPredicateKey::BlockNumber => {
                    ValidityPredicate::BlockNumber { op, value: U256::from(value) }
                }
                _ => ValidityPredicate::FlashblockIndex { op, value: U256::from(value) },
            };
            let mut db = RecordingDatabase { fail_on: Some(1), ..Default::default() };
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context),
                Ok(ValidityPredicateEvaluation::Unsatisfied {
                    blocker,
                    predicate: predicate.clone(),
                    expired
                })
            );
            assert!(conditions.contains(&predicate));
            assert!(db.reads.is_empty());
        }
        let mut db = RecordingDatabase { fail_on: Some(1), ..Default::default() };
        let context = PredicateContext { block_number: 11, flashblock_index: 2 };
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context),
            Err(ReadError)
        );
        assert_eq!(db.reads, [(Address::ZERO, None)]);
        db.reads.clear();
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(ValidityConditions::empty(), &mut db, &context),
            Ok(ValidityPredicateEvaluation::Matched)
        );
        assert!(db.reads.is_empty());
    }

    #[test]
    fn timing_mismatch_parks_without_reading_state() {
        let address = Address::with_last_byte(1);
        let conditions = ValidityConditions::new(vec![
            balance(address, ValidityOperator::GreaterThan, 0),
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(101),
            },
        ]);
        let context = PredicateContext { block_number: 100, flashblock_index: 1 };
        let mut db = InMemoryDB::default();
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &context).unwrap(),
            ValidityPredicateEvaluation::Unsatisfied {
                blocker: ValidityPredicateKey::BlockNumber,
                predicate: ValidityPredicate::BlockNumber {
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(101)
                },
                expired: false,
            },
        );
        assert!(db.cache.accounts.is_empty(), "timing gate must not load account state");
        let next = PredicateContext { block_number: 101, ..context };
        let evaluation =
            ValidityPredicateEvaluation::evaluate(&conditions, &mut db, &next).unwrap();
        assert!(
            matches!(evaluation, ValidityPredicateEvaluation::Unsatisfied { blocker: ValidityPredicateKey::Balance(found), .. } if found == address)
        );
        assert!(db.cache.accounts.contains_key(&address));
    }

    #[test]
    fn passed_nonce_upper_bounds_remain_ineligible_until_block_expiry() {
        let address = Address::with_last_byte(1);
        let context = PredicateContext { block_number: 100, flashblock_index: 1 };
        for (op, initial_nonce) in [
            (ValidityOperator::Equal, 3),
            (ValidityOperator::LessThan, 2),
            (ValidityOperator::LessThanOrEqual, 3),
        ] {
            let predicates = [
                ValidityPredicate::Nonce { address, op, value: U256::from(3) },
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(102),
                },
            ];
            let predicates = ValidityConditions::new(predicates.to_vec());
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                address,
                AccountInfo { nonce: initial_nonce, ..Default::default() },
            );
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&predicates, &mut db, &context).unwrap(),
                ValidityPredicateEvaluation::Matched,
            );
            db.insert_account_info(address, AccountInfo { nonce: 4, ..Default::default() });
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&predicates, &mut db, &context).unwrap(),
                ValidityPredicateEvaluation::Unsatisfied {
                    blocker: ValidityPredicateKey::Nonce(address),
                    predicate: ValidityPredicate::Nonce { address, op, value: U256::from(3) },
                    expired: false,
                },
            );
            let expired_context = PredicateContext { block_number: 103, ..context };
            assert_eq!(
                ValidityPredicateEvaluation::evaluate(&predicates, &mut db, &expired_context)
                    .unwrap(),
                ValidityPredicateEvaluation::Unsatisfied {
                    blocker: ValidityPredicateKey::BlockNumber,
                    predicate: ValidityPredicate::BlockNumber {
                        op: ValidityOperator::LessThanOrEqual,
                        value: U256::from(102)
                    },
                    expired: true,
                },
            );
        }
    }

    #[test]
    fn deleted_accounts_with_zero_original_nonce_do_not_wake_riders() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        for threshold in [1, 32] {
            let mut index = ParkedPredicateIndex::new(threshold);
            index.park(
                hash,
                (),
                ValidityPredicate::Nonce {
                    address,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::ONE,
                },
            );
            let mut account = Account::default();
            account.info.nonce = 1;
            account.mark_selfdestruct();
            assert_eq!(
                index.affected_by_state(&EvmState::from_iter([(address, account)])),
                StateChangeEffects::default(),
            );
        }
    }

    #[test]
    fn nonce_buckets_wake_on_nonce_changes_only() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        for threshold in [1, 32] {
            let mut index = ParkedPredicateIndex::new(threshold);
            index.park(
                hash,
                (),
                ValidityPredicate::Nonce {
                    address,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(10),
                },
            );
            assert_eq!(
                index.affected_by_state(&changed_balance(address, 9, 10)),
                StateChangeEffects::default(),
            );
            let mut account = Account::default();
            account.original_info_mut().nonce = 9;
            account.info.nonce = 10;
            assert_eq!(
                index.affected_by_state(&EvmState::from_iter([(address, account)])),
                StateChangeEffects { affected_transactions: vec![hash], woken_buckets: 1 },
            );
            assert_eq!(index.remove(hash), Some(()));
            assert!(index.is_empty());
        }
    }

    #[test]
    fn ordered_nonce_buckets_wake_each_operator_at_its_boundary() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        for (op, old, new) in [
            (ValidityOperator::LessThan, 10, 9),
            (ValidityOperator::LessThanOrEqual, 11, 10),
            (ValidityOperator::Equal, 9, 10),
            (ValidityOperator::NotEqual, 10, 11),
            (ValidityOperator::GreaterThan, 10, 11),
            (ValidityOperator::GreaterThanOrEqual, 9, 10),
        ] {
            let mut index = ParkedPredicateIndex::new(1);
            index.park(hash, (), ValidityPredicate::Nonce { address, op, value: U256::from(10) });
            let mut account = Account::default();
            account.original_info_mut().nonce = old;
            account.info.nonce = new;
            assert_eq!(
                index
                    .affected_by_state(&EvmState::from_iter([(address, account)]))
                    .affected_transactions,
                vec![hash],
            );
        }
    }

    #[test]
    fn ordered_nonce_bucket_skips_uncrossed_threshold_and_wakes_on_deletion() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::new(1);
        index.park(
            hash,
            (),
            ValidityPredicate::Nonce { address, op: ValidityOperator::Equal, value: U256::ZERO },
        );
        index.park(
            B256::with_last_byte(2),
            (),
            ValidityPredicate::Nonce { address, op: ValidityOperator::Equal, value: U256::from(5) },
        );
        let mut account = Account::default();
        account.original_info_mut().nonce = 9;
        account.info.nonce = 10;
        assert!(
            index
                .affected_by_state(&EvmState::from_iter([(address, account.clone())]))
                .affected_transactions
                .is_empty()
        );
        account.mark_selfdestruct();
        assert_eq!(
            index
                .affected_by_state(&EvmState::from_iter([(address, account)]))
                .affected_transactions,
            vec![hash],
        );
    }

    #[test]
    fn flat_bucket_wakes_every_rider() {
        let address = Address::with_last_byte(1);
        let hashes = [B256::with_last_byte(1), B256::with_last_byte(2)];
        let mut index = ParkedPredicateIndex::new(3);
        for hash in hashes {
            index.park(hash, (), balance(address, ValidityOperator::GreaterThan, 10));
        }
        let effects = index.affected_by_state(&changed_balance(address, 1, 2));
        assert_eq!(effects.woken_buckets, 1);
        assert_eq!(
            effects.affected_transactions.into_iter().collect::<B256Set>(),
            B256Set::from_iter(hashes)
        );
    }

    #[test]
    fn ordered_bucket_skips_uncrossed_thresholds_and_wakes_crossed_ones() {
        let address = Address::with_last_byte(1);
        let first = B256::with_last_byte(1);
        let second = B256::with_last_byte(2);
        let mut index = ParkedPredicateIndex::new(2);
        index.park(first, (), balance(address, ValidityOperator::GreaterThanOrEqual, 10));
        index.park(second, (), balance(address, ValidityOperator::GreaterThanOrEqual, 20));
        assert!(
            index
                .affected_by_state(&changed_balance(address, 1, 9))
                .affected_transactions
                .is_empty()
        );
        assert_eq!(
            index.affected_by_state(&changed_balance(address, 9, 12)).affected_transactions,
            vec![first]
        );
        assert_eq!(
            index.affected_by_state(&changed_balance(address, 12, 25)).affected_transactions,
            vec![second]
        );
    }

    #[test]
    fn ordered_bucket_wakes_each_threshold_operator_at_its_boundary() {
        let address = Address::with_last_byte(1);
        for (hash, op, old, new) in [
            (B256::with_last_byte(1), ValidityOperator::LessThan, 10, 9),
            (B256::with_last_byte(2), ValidityOperator::LessThanOrEqual, 11, 10),
            (B256::with_last_byte(3), ValidityOperator::GreaterThan, 10, 11),
        ] {
            let mut index = ParkedPredicateIndex::new(1);
            index.park(hash, (), balance(address, op, 10));
            assert_eq!(
                index.affected_by_state(&changed_balance(address, old, new)).affected_transactions,
                vec![hash]
            );
        }
    }

    #[test]
    fn ordered_bucket_wakes_masked_storage_point_operators() {
        let address = Address::with_last_byte(1);
        let equal = B256::with_last_byte(1);
        let not_equal = B256::with_last_byte(2);
        let mask = U256::from(0xff);

        let mut index = ParkedPredicateIndex::new(1);
        index.park(equal, (), storage(address, mask, ValidityOperator::Equal, 10));
        assert_eq!(
            index.affected_by_state(&changed_storage(address, 0x109, 0x20a)).affected_transactions,
            vec![equal]
        );

        let mut index = ParkedPredicateIndex::new(1);
        index.park(not_equal, (), storage(address, mask, ValidityOperator::NotEqual, 10));
        assert_eq!(
            index.affected_by_state(&changed_storage(address, 0x10a, 0x20b)).affected_transactions,
            vec![not_equal]
        );
    }

    #[test]
    fn reindex_and_remove_update_ordered_buckets() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::new(1);
        index.park(hash, 7, balance(address, ValidityOperator::GreaterThan, 10));
        assert!(index.reindex(hash, balance(address, ValidityOperator::GreaterThan, 20)));
        assert!(
            index
                .affected_by_state(&changed_balance(address, 9, 11))
                .affected_transactions
                .is_empty()
        );
        assert_eq!(index.remove(hash), Some(7));
        assert_eq!(
            index.affected_by_state(&changed_balance(address, 19, 21)),
            StateChangeEffects::default()
        );
    }

    #[test]
    fn ordered_bucket_retains_never_satisfied_predicates() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::new(1);
        index.park(
            hash,
            7,
            ValidityPredicate::Balance {
                address,
                op: ValidityOperator::GreaterThan,
                value: U256::MAX,
            },
        );
        assert_eq!(index.bucket_depths().collect::<Vec<_>>(), vec![1]);
        assert!(
            index
                .affected_by_state(&changed_balance(address, 0, 1))
                .affected_transactions
                .is_empty()
        );
        assert_eq!(index.remove(hash), Some(7));
        assert!(index.is_empty());
    }

    #[test]
    fn selfdestruct_conservatively_wakes_ordered_storage_buckets() {
        let address = Address::with_last_byte(1);
        let hash = B256::with_last_byte(1);
        let mut index = ParkedPredicateIndex::new(1);
        index.park(hash, (), storage(address, U256::MAX, ValidityOperator::GreaterThan, 10));
        let mut account = Account::default();
        account.mark_selfdestruct();
        let effects = index.affected_by_state(&EvmState::from_iter([(address, account)]));
        assert_eq!(
            effects,
            StateChangeEffects { affected_transactions: vec![hash], woken_buckets: 1 }
        );
    }
}
