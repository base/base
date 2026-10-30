//! Deterministic instruction-count benchmark for compiled validity conditions (validity predicate
//! batches) in the native payload builder.
//!
//! The `iai-callgrind` counterpart to the evaluation and ingestion arms of
//! `validity_conditions.rs`, over a pool-sized candidate set rather than one hot batch.
//!
//! - `evaluate`: the candidate loop's predicate check. For each candidate,
//!   `execute_best_transactions` wraps the build state in a [`PredicateReadRecorder`] and calls
//!   [`ValidityPredicateEvaluation::evaluate`]. That runs once per validity candidate in every
//!   200 ms block after the Denim network upgrade, and again whenever a parked candidate is
//!   rescanned, so it is on the per-candidate hot path (about 10^9 candidates a day; see
//!   `docs/guides/HOT_PATH_PERFORMANCE_REVIEW.md`). The state is a fresh revm `State` over an
//!   in-memory database, so first reads load into the per-build cache as in a real build.
//!   - `pass`: every candidate's predicates hold.
//!   - `mixed`: one in eight candidates has passed its block expiry and two in eight fail their
//!     last state predicate, so the run covers matched, expired, and parked outcomes.
//! - `ingest`: compiling JSON-decoded batches into [`ValidityConditions`], which pool admission
//!   does once per submitted validity transaction (`BasePooledTransaction::with_validity`).
//!
//! Each candidate carries a block-number expiry, which every validity transaction needs, plus one
//! to four state predicates rotating through balance, nonce, and storage, with the expiry at a
//! varying wire position. Balance and nonce predicates read the candidate's own accounts; storage
//! predicates read distinct slots of one shared contract, as watchers of a popular protocol do.
//! Flashblock-index predicates are omitted because the native builder rejects them before
//! evaluation.
//!
//! The teardowns pin the evaluation outcomes and the predicate read footprint, or the compiled
//! predicate counts. They are fixed by the workload, so a mismatch means evaluation or
//! compilation now behaves differently, and the instruction count is not comparable with the base
//! commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use alloy_primitives::{Address, U256, keccak256};
use base_execution_payload_builder::{
    PredicateLoadTracker, PredicateReadRecorder, ValidityPredicateEvaluation,
};
use base_execution_txpool::{
    PredicateContext, TransactionValidity, ValidityConditions, ValidityOperator, ValidityPredicate,
};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_revm::db::State;
use revm::{database::InMemoryDB, state::AccountInfo};

/// Candidates per run, the size of the criterion bench's pool-scale set.
const CANDIDATES: usize = 10_000;
/// Most state predicates one candidate carries.
const MAX_STATE_PREDICATES: usize = 4;
/// Number of the block being built.
const BLOCK_NUMBER: u64 = 500;
/// Block expiry of live candidates.
const EXPIRY_BLOCK: u64 = 1_000;
const NONCE: u64 = 7;
const BALANCE: u64 = 1_000;
/// Contract whose slots the storage predicates watch.
const WATCHED_CONTRACT: Address = Address::repeat_byte(0xc0);
/// The native builder evaluates every candidate at flashblock index zero.
const CONTEXT: PredicateContext =
    PredicateContext { block_number: BLOCK_NUMBER, flashblock_index: 0 };

/// How a candidate's predicates evaluate against the build state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    Matched,
    /// The last state predicate fails, so the candidate parks.
    Unsatisfied,
    /// The block expiry has passed.
    Expired,
}

/// Which outcomes the candidate set mixes.
#[derive(Debug, Clone, Copy)]
enum Workload {
    Pass,
    Mixed,
}

impl Workload {
    const fn expected(self, index: usize) -> Expected {
        match (self, index % 8) {
            (Self::Mixed, 0) => Expected::Expired,
            (Self::Mixed, 1 | 2) => Expected::Unsatisfied,
            _ => Expected::Matched,
        }
    }

    /// Wire-order predicate batch of candidate `index`.
    fn batch(self, index: usize) -> Vec<ValidityPredicate> {
        let expected = self.expected(index);
        let state = 1 + index % MAX_STATE_PREDICATES;
        let mut predicates = Vec::with_capacity(state + 1);
        for position in 0..state {
            let satisfied = !(expected == Expected::Unsatisfied && position + 1 == state);
            // Each predicate reads its own account or slot.
            let location = index * MAX_STATE_PREDICATES + position;
            let account = account(location);
            predicates.push(match (index + position) % 3 {
                0 => ValidityPredicate::Storage {
                    address: WATCHED_CONTRACT,
                    slot: slot(location),
                    mask: U256::MAX,
                    op: ValidityOperator::Equal,
                    value: if satisfied { slot_value(location) } else { U256::MAX },
                },
                1 => ValidityPredicate::Balance {
                    address: account,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(if satisfied { 1 } else { BALANCE + 1 }),
                },
                _ => ValidityPredicate::Nonce {
                    address: account,
                    op: ValidityOperator::Equal,
                    value: U256::from(if satisfied { NONCE } else { NONCE + 1 }),
                },
            });
        }
        let expiry = if expected == Expected::Expired { BLOCK_NUMBER - 1 } else { EXPIRY_BLOCK };
        predicates.insert(
            index % (state + 1),
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(expiry),
            },
        );
        predicates
    }

    /// State under which every satisfied predicate of the candidate set holds.
    fn database(self) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        let info = AccountInfo { balance: U256::from(BALANCE), nonce: NONCE, ..Default::default() };
        db.insert_account_info(WATCHED_CONTRACT, info.clone());
        for index in 0..CANDIDATES {
            for position in 0..MAX_STATE_PREDICATES {
                let location = index * MAX_STATE_PREDICATES + position;
                db.insert_account_info(account(location), info.clone());
                db.insert_account_storage(WATCHED_CONTRACT, slot(location), slot_value(location))
                    .expect("in-memory insert");
            }
        }
        db
    }
}

/// A hash-derived address, so hash maps see keys distributed like real addresses whatever the
/// process's random hash seed; sequential small keys cluster under some seeds, which makes the
/// instruction count vary from run to run.
fn account(location: usize) -> Address {
    Address::from_word(keccak256(location.to_be_bytes()))
}

/// A hash-derived slot, as mapping slots are, for the same reason as [`account`].
fn slot(location: usize) -> U256 {
    keccak256(account(location)).into()
}

fn slot_value(location: usize) -> U256 {
    U256::from(location) + U256::ONE
}

/// Decodes a batch from JSON the way `base_sendRawTransactionValidity` does.
fn json_decoded(predicates: Vec<ValidityPredicate>) -> Vec<ValidityPredicate> {
    let json = serde_json::to_string(&TransactionValidity {
        validity: predicates,
        validity_signature: None,
    })
    .expect("serializes");
    serde_json::from_str::<TransactionValidity>(&json).expect("deserializes").validity
}

/// Evaluation outcomes and read footprint of one pass over the candidate set.
#[derive(Debug, Default, PartialEq, Eq)]
struct Outcomes {
    matched: usize,
    unsatisfied: usize,
    expired: usize,
    account_reads: u64,
    slot_reads: u64,
    unique_accounts: u64,
    unique_slots: u64,
}

/// Compiled candidates and the build state they are evaluated against.
type EvaluateInput = (Workload, Vec<ValidityConditions>, State<InMemoryDB>, PredicateLoadTracker);

/// Compiles the candidate set and builds fresh state outside the counted region.
fn setup_evaluate(workload: Workload) -> EvaluateInput {
    let conditions =
        (0..CANDIDATES).map(|index| ValidityConditions::new(workload.batch(index))).collect();
    let db = State::builder().with_database(workload.database()).with_bundle_update().build();
    (workload, conditions, db, PredicateLoadTracker::default())
}

/// Checks the evaluation outcomes against the workload's documented counts.
fn check_evaluate((outcomes, (workload, ..)): (Outcomes, EvaluateInput)) {
    let expected = match workload {
        // Every state predicate is read once: 16,666 balance or nonce and 8,334 storage.
        Workload::Pass => Outcomes {
            matched: 10_000,
            unsatisfied: 0,
            expired: 0,
            account_reads: 16_666,
            slot_reads: 8_334,
            unique_accounts: 16_666,
            unique_slots: 8_334,
        },
        // Expired candidates read nothing, and parked ones stop at their failing predicate.
        Workload::Mixed => Outcomes {
            matched: 6_250,
            unsatisfied: 2_500,
            expired: 1_250,
            account_reads: 15_416,
            slot_reads: 6_667,
            unique_accounts: 15_416,
            unique_slots: 6_667,
        },
    };
    assert_eq!(outcomes, expected, "validity_conditions/evaluate {workload:?} outcomes changed");
}

#[library_benchmark]
#[bench::pass(args = (Workload::Pass), setup = setup_evaluate, teardown = check_evaluate)]
#[bench::mixed(args = (Workload::Mixed), setup = setup_evaluate, teardown = check_evaluate)]
fn evaluate(mut input: EvaluateInput) -> (Outcomes, EvaluateInput) {
    let mut outcomes = Outcomes::default();
    for conditions in &input.1 {
        // As in `execute_best_transactions`: a fresh recorder per candidate over the build state.
        let evaluation = {
            let mut recorder = PredicateReadRecorder::new(&mut input.2, &mut input.3);
            ValidityPredicateEvaluation::evaluate(conditions, &mut recorder, &CONTEXT)
        };
        match evaluation.expect("in-memory reads cannot fail") {
            ValidityPredicateEvaluation::Matched => outcomes.matched += 1,
            ValidityPredicateEvaluation::Unsatisfied { expired: true, .. } => outcomes.expired += 1,
            ValidityPredicateEvaluation::Unsatisfied { expired: false, .. } => {
                outcomes.unsatisfied += 1;
            }
        }
    }
    outcomes.account_reads = input.3.account_reads();
    outcomes.slot_reads = input.3.slot_reads();
    outcomes.unique_accounts = input.3.unique_accounts();
    outcomes.unique_slots = input.3.unique_slots();
    (black_box(outcomes), input)
}

/// JSON-decoded batches of the `pass` candidate set, built outside the counted region.
fn setup_ingest() -> Vec<Vec<ValidityPredicate>> {
    (0..CANDIDATES).map(|index| json_decoded(Workload::Pass.batch(index))).collect()
}

/// Checks that every submitted predicate was compiled into its partition.
fn check_ingest(compiled: Vec<ValidityConditions>) {
    let submitted: usize = compiled.iter().map(ValidityConditions::len).sum();
    let state: usize =
        compiled.iter().map(|conditions| conditions.state_predicates().count()).sum();
    let expiring = compiled.iter().filter(|conditions| conditions.block().max().is_some()).count();
    assert_eq!(
        (compiled.len(), submitted, state, expiring),
        (CANDIDATES, 35_000, 25_000, CANDIDATES),
        "validity_conditions/ingest compiled predicates changed"
    );
}

#[library_benchmark]
#[bench::json_decoded(setup = setup_ingest, teardown = check_ingest)]
fn ingest(batches: Vec<Vec<ValidityPredicate>>) -> Vec<ValidityConditions> {
    black_box(batches.into_iter().map(ValidityConditions::new).collect())
}

library_benchmark_group!(
    name = validity_conditions;
    benchmarks = evaluate, ingest
);

main!(library_benchmark_groups = validity_conditions);
