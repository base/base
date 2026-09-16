//! Per-block accounting of payload validity-predicate state loads.
//!
//! Evaluating a transaction's validity predicates reads account balances and
//! contract storage slots from the state the builder is building on. This module
//! measures the *footprint* of those reads per block — how many accounts and
//! storage slots predicate evaluation touches, both in total (every read) and
//! deduplicated (distinct locations).
//!
//! It deliberately does **not** classify reads as cache hits vs disk misses.
//! The read flows through several cache tiers (the per-build revm `State` cache,
//! reth's cross-block execution cache, then MDBX), and the only tier that
//! cleanly represents "went to disk" is the execution-cache miss — which is not
//! observable from here and is already measured, un-scoped, by reth's
//! `sync.caching` metrics. The footprint counted here is the signal that a
//! predicate-state prewarmer must cover and that an abusive submitter inflates.
//!
//! [`PredicateReadRecorder`] wraps the builder's [`Database`] during a predicate
//! evaluation and records each read into a [`PredicateLoadTracker`] that
//! accumulates across the whole block build. Because reads flow through the
//! wrapper as the evaluator issues them, short-circuit evaluation is respected:
//! only the reads a predicate check actually performs are counted.

use alloy_primitives::{Address, B256, U256, map::HashSet};
use base_execution_txpool::PredicateDatabase;
use revm::{
    Database,
    state::{AccountInfo, Bytecode},
};

/// Per-block accumulator for validity-predicate state loads.
///
/// Totals count every read: a slot re-read (e.g. after it is written, or when a
/// parked transaction is re-evaluated after a state change) counts again, so the
/// total captures raw read volume. The unique sets count distinct locations touched
/// across the block, i.e. the predicate state footprint.
#[derive(Debug, Default)]
pub struct PredicateLoadTracker {
    /// Total account (balance) reads.
    account_reads: u64,
    /// Total storage-slot reads.
    slot_reads: u64,
    /// Distinct accounts read this block.
    unique_accounts: HashSet<Address>,
    /// Distinct storage slots read this block.
    unique_slots: HashSet<(Address, U256)>,
}

impl PredicateLoadTracker {
    /// Records a single account (balance) read.
    pub fn record_account(&mut self, address: Address) {
        self.account_reads += 1;
        self.unique_accounts.insert(address);
    }

    /// Records a single storage-slot read.
    pub fn record_slot(&mut self, address: Address, slot: U256) {
        self.slot_reads += 1;
        self.unique_slots.insert((address, slot));
    }

    /// Returns whether any predicate read was recorded this block.
    ///
    /// Used to gate metric emission so blocks that carried no validity
    /// transactions do not flood the per-block histograms with zero observations.
    pub const fn has_activity(&self) -> bool {
        self.account_reads > 0 || self.slot_reads > 0
    }

    /// Total account reads this block.
    pub const fn account_reads(&self) -> u64 {
        self.account_reads
    }

    /// Total storage-slot reads this block.
    pub const fn slot_reads(&self) -> u64 {
        self.slot_reads
    }

    /// Distinct accounts read this block.
    pub fn unique_accounts(&self) -> u64 {
        self.unique_accounts.len() as u64
    }

    /// Distinct storage slots read this block.
    pub fn unique_slots(&self) -> u64 {
        self.unique_slots.len() as u64
    }
}

/// A [`Database`] wrapper that records validity-predicate state reads.
///
/// Wraps the builder's [`Database`] for the duration of a predicate evaluation and
/// records each [`PredicateDatabase::balance`] and [`Database::storage`] call into
/// the shared [`PredicateLoadTracker`] before delegating. It does not inspect any
/// cache — it only counts which locations predicate evaluation reads, respecting
/// short-circuit evaluation because reads flow through it as they are issued.
#[derive(Debug)]
pub struct PredicateReadRecorder<'a, DB> {
    database: &'a mut DB,
    tracker: &'a mut PredicateLoadTracker,
}

impl<'a, DB> PredicateReadRecorder<'a, DB> {
    /// Wraps `database`, recording reads into `tracker`.
    pub const fn new(database: &'a mut DB, tracker: &'a mut PredicateLoadTracker) -> Self {
        Self { database, tracker }
    }
}

impl<DB: PredicateDatabase> PredicateDatabase for PredicateReadRecorder<'_, DB> {
    fn balance(&mut self, address: Address) -> Result<U256, Self::Error> {
        self.tracker.record_account(address);
        self.database.balance(address)
    }
}

impl<DB: Database> Database for PredicateReadRecorder<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.tracker.record_account(address);
        self.database.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.database.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.tracker.record_slot(address, index);
        self.database.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.database.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, sync::Arc};

    use alloy_primitives::{Address, B256, U256};
    use base_execution_txpool::{
        PredicateContext, PredicateDatabase, ValidityOperator, ValidityPredicate,
    };
    use reth_revm::State;
    use revm::{
        Database, DatabaseCommit,
        database::{BundleState, DBErrorMarker, InMemoryDB},
        state::{
            Account, AccountInfo, Bytecode,
            bal::{AccountBal, Bal},
        },
    };

    use super::{PredicateLoadTracker, PredicateReadRecorder};
    use crate::{ValidityPredicateEvaluation, ValidityPredicateKey};

    #[derive(Debug)]
    struct ReadError;

    impl fmt::Display for ReadError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("read failed")
        }
    }

    impl std::error::Error for ReadError {}
    impl DBErrorMarker for ReadError {}

    #[derive(Debug)]
    struct FailingDatabase;

    impl Database for FailingDatabase {
        type Error = ReadError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ReadError)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Err(ReadError)
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(ReadError)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Err(ReadError)
        }
    }

    fn context() -> PredicateContext {
        PredicateContext { block_number: 1, flashblock_index: 0 }
    }

    /// Builds a `State` over an in-memory database seeded with one account that
    /// has a balance and a single storage slot.
    fn seeded_state() -> (State<InMemoryDB>, Address, U256) {
        let address = Address::with_last_byte(0x11);
        let slot = U256::from(7);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            address,
            AccountInfo { balance: U256::from(10), ..Default::default() },
        );
        db.insert_account_storage(address, slot, U256::from(3)).unwrap();
        let state = State::builder().with_database(db).with_bundle_update().build();
        (state, address, slot)
    }

    #[test]
    fn counts_account_and_slot_reads_total_and_unique() {
        let (mut state, address, slot) = seeded_state();
        let mut tracker = PredicateLoadTracker::default();

        let balance = ValidityPredicate::Balance {
            address,
            op: ValidityOperator::Equal,
            value: U256::from(10),
        };
        let storage = ValidityPredicate::Storage {
            address,
            slot,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(3),
        };
        let predicates = [balance, storage];

        // Evaluate the same batch twice: totals double, unique counts do not.
        for _ in 0..2 {
            let mut recorder = PredicateReadRecorder::new(&mut state, &mut tracker);
            assert_eq!(
                ValidityPredicateKey::first_unsatisfied(&predicates, &mut recorder, &context())
                    .unwrap(),
                None
            );
        }

        assert_eq!(tracker.account_reads(), 2);
        assert_eq!(tracker.slot_reads(), 2);
        assert_eq!(tracker.unique_accounts(), 1);
        assert_eq!(tracker.unique_slots(), 1);
        assert!(tracker.has_activity());
    }

    #[test]
    fn short_circuit_skips_reads_after_first_unsatisfied_predicate() {
        let (mut state, address, slot) = seeded_state();
        let mut tracker = PredicateLoadTracker::default();

        // The balance predicate fails, so evaluation stops before the storage
        // predicate is ever read.
        let failing_balance = ValidityPredicate::Balance {
            address,
            op: ValidityOperator::Equal,
            value: U256::from(999),
        };
        let storage = ValidityPredicate::Storage {
            address,
            slot,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(3),
        };
        let predicates = [failing_balance, storage];

        {
            let mut recorder = PredicateReadRecorder::new(&mut state, &mut tracker);
            assert_eq!(
                ValidityPredicateKey::first_unsatisfied(&predicates, &mut recorder, &context())
                    .unwrap(),
                Some(ValidityPredicateKey::Balance(address))
            );
        }

        assert_eq!(tracker.account_reads(), 1);
        assert_eq!(tracker.slot_reads(), 0, "storage slot must not be read after short circuit");
        assert_eq!(tracker.unique_accounts(), 1);
        assert_eq!(tracker.unique_slots(), 0);
    }

    #[test]
    fn balance_reads_current_committed_and_destroyed_state() {
        let address = Address::with_last_byte(0x22);
        let mut state = State::builder().with_database(InMemoryDB::default()).build();
        let mut tracker = PredicateLoadTracker::default();

        let mut changed = Account::default();
        changed.info.balance = U256::from(42);
        changed.mark_touch();
        state.commit([(address, changed)].into_iter().collect());
        assert_eq!(
            PredicateReadRecorder::new(&mut state, &mut tracker).balance(address),
            Ok(U256::from(42))
        );

        let mut destroyed = Account::default();
        destroyed.mark_touch();
        destroyed.mark_selfdestruct();
        state.commit([(address, destroyed)].into_iter().collect());
        assert_eq!(
            PredicateReadRecorder::new(&mut state, &mut tracker).balance(address),
            Ok(U256::ZERO)
        );
        assert_eq!(tracker.account_reads(), 2);
    }

    #[test]
    fn balance_reads_preloaded_bundle_and_absent_account() {
        let present = Address::with_last_byte(0x33);
        let absent = Address::with_last_byte(0x44);
        let bundle = BundleState::builder(0..=0)
            .state_present_account_info(
                present,
                AccountInfo { balance: U256::from(99), ..Default::default() },
            )
            .build();
        let mut state = State::builder()
            .with_database(InMemoryDB::default())
            .with_bundle_prestate(bundle)
            .build();
        let mut tracker = PredicateLoadTracker::default();
        let mut recorder = PredicateReadRecorder::new(&mut state, &mut tracker);

        assert_eq!(recorder.balance(present), Ok(U256::from(99)));
        assert_eq!(recorder.balance(absent), Ok(U256::ZERO));
    }

    #[test]
    fn balance_preserves_bal_consumer_and_builder_semantics() {
        let address = Address::with_last_byte(0x55);
        let mut database = InMemoryDB::default();
        database.insert_account_info(
            address,
            AccountInfo { balance: U256::from(7), ..Default::default() },
        );
        let bal = Arc::new(Bal::from_iter([(address, AccountBal::default())]));
        let mut state =
            State::builder().with_database(database).with_bal(bal).with_bal_builder().build();
        let expected = state.basic(address).unwrap().unwrap().balance;
        let mut tracker = PredicateLoadTracker::default();

        assert_eq!(
            PredicateReadRecorder::new(&mut state, &mut tracker).balance(address),
            Ok(expected)
        );
        assert!(
            PredicateReadRecorder::new(&mut state, &mut tracker)
                .balance(Address::with_last_byte(0x56))
                .is_err()
        );
        assert!(state.take_built_bal().unwrap().accounts.is_empty());
        assert_eq!(tracker.account_reads(), 2);
    }

    #[test]
    fn state_evaluation_preserves_expired_context_predicates_without_reads() {
        let mut state = State::builder().with_database(InMemoryDB::default()).build();
        let mut tracker = PredicateLoadTracker::default();
        let context = PredicateContext { block_number: 10, flashblock_index: 3 };
        let block_number = [ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThan,
            value: U256::from(context.block_number),
        }];
        let mut recorder = PredicateReadRecorder::new(&mut state, &mut tracker);
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(&block_number, &mut recorder, &context).unwrap(),
            ValidityPredicateEvaluation::Unsatisfied {
                blocker: ValidityPredicateKey::BlockNumber,
                expired: true,
            }
        );

        let flashblock_index = [
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::Equal,
                value: U256::from(context.block_number),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThan,
                value: U256::from(context.flashblock_index),
            },
        ];
        let mut recorder = PredicateReadRecorder::new(&mut state, &mut tracker);
        assert_eq!(
            ValidityPredicateEvaluation::evaluate(&flashblock_index, &mut recorder, &context)
                .unwrap(),
            ValidityPredicateEvaluation::Unsatisfied {
                blocker: ValidityPredicateKey::FlashblockIndex,
                expired: true,
            }
        );
        assert_eq!(tracker.account_reads(), 0);
        assert_eq!(tracker.slot_reads(), 0);
    }

    #[test]
    fn failed_balance_read_is_counted_and_propagated() {
        let address = Address::with_last_byte(0x66);
        let mut state = State::builder().with_database(FailingDatabase).build();
        let mut tracker = PredicateLoadTracker::default();

        assert!(PredicateReadRecorder::new(&mut state, &mut tracker).balance(address).is_err());
        assert_eq!(tracker.account_reads(), 1);
    }

    #[test]
    fn empty_tracker_reports_no_activity() {
        assert!(!PredicateLoadTracker::default().has_activity());
    }
}
