//! Benchmarks for compiled validity conditions (validity predicate batches): ingestion,
//! repeated evaluation in the candidate loop, resting/parking membership checks, and
//! builder forwarding, plus a retained-memory report per predicate mix.
//!
//! Timing predicates are interleaved with state predicates rather than leading the batch,
//! so the benchmark exercises wire orders where the block-number predicate is not first.
//!
//! The memory table is printed before the timed benchmarks. It reports
//! `size_of::<ValidityConditions>() + heap_size()` next to the bytes requested from the
//! allocator while compiling the batch, and the pool-accounted size of a transfer carrying
//! that batch after JSON decoding, which is how the RPC path ingests it.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench validity_conditions
//! ```

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use alloy_consensus::{InMemorySize, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_payload_builder::ValidityPredicateKey;
use base_execution_txpool::{
    BasePooledTransaction, PredicateContext, TransactionValidity, ValidatedTransactionExtensions,
    ValidityConditions, ValidityOperator, ValidityPredicate,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use evm2::evm::{AccountInfo, InMemoryDB};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{TransactionOrigin, ValidPoolTransaction, identifier::TransactionId};

/// Counts allocations so the report can show real allocator traffic next to `heap_size`.
struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` unchanged and only bumps relaxed counters, so the
// allocator contract is exactly that of `System`.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller upholds the `GlobalAlloc::alloc` contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by `System` through this allocator with the same `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
        // SAFETY: `ptr` was returned by `System` through this allocator with `layout`, and the
        // caller upholds the `GlobalAlloc::realloc` contract for `new_size`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    let count = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let value = f();
    (
        value,
        ALLOCATIONS.load(Ordering::Relaxed) - count,
        ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes,
    )
}

const EXPIRY_BLOCK: u64 = 1_000;
const NONCE: u64 = 7;
const BALANCE: u64 = 1_000;
const CONTEXT: PredicateContext = PredicateContext { block_number: 500, flashblock_index: 3 };
const EXPIRED_CONTEXT: PredicateContext =
    PredicateContext { block_number: EXPIRY_BLOCK + 1, flashblock_index: 3 };
/// Wire ceiling for one batch.
const MAX_PREDICATES: usize = 64;
/// Transactions in the pool-scale evaluation benchmark.
const POOL_TRANSACTIONS: usize = 10_000;

fn address(index: usize) -> Address {
    Address::from_word(U256::from(index + 1).into())
}

fn state_predicate(kind: usize, address: Address, satisfied: bool) -> ValidityPredicate {
    match kind % 3 {
        0 => ValidityPredicate::Storage {
            address,
            slot: U256::from(kind),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: if satisfied { U256::from(kind) } else { U256::MAX },
        },
        1 => ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(if satisfied { 1 } else { BALANCE + 1 }),
        },
        _ => ValidityPredicate::Nonce {
            address,
            op: ValidityOperator::Equal,
            value: if satisfied { U256::from(NONCE) } else { U256::from(NONCE + 1) },
        },
    }
}

/// A batch with `state` state predicates rotating through storage, balance, and nonce, and
/// `timing` timing predicates (one block expiry, then flashblock lower bounds) interleaved
/// among them.
///
/// The block expiry is inserted about two thirds of the way through the state predicates and
/// the flashblock bounds about one third of the way, both shifted by `rotation`, so the
/// block-number predicate is never simply first and pool-scale workloads vary the position.
///
/// State addresses start at `address_base` so pool-scale workloads can use distinct accounts.
/// When `last_fails` is set, the final state predicate is unsatisfied against [`database`].
fn mixed(
    timing: usize,
    state: usize,
    address_base: usize,
    rotation: usize,
    last_fails: bool,
) -> Vec<ValidityPredicate> {
    let mut predicates = Vec::with_capacity(timing + state);
    for kind in 0..state {
        let satisfied = !(last_fails && kind + 1 == state);
        predicates.push(state_predicate(kind, address(address_base + kind), satisfied));
    }
    if timing > 0 {
        let at = ((2 * state).div_ceil(3) + rotation) % (predicates.len() + 1);
        predicates.insert(
            at,
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(EXPIRY_BLOCK),
            },
        );
    }
    for _ in 1..timing {
        let at = (state / 3 + rotation) % (predicates.len() + 1);
        predicates.insert(
            at,
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::ONE,
            },
        );
    }
    predicates
}

/// State under which every satisfied predicate from [`mixed`] holds.
fn database(predicates: impl IntoIterator<Item = ValidityPredicate>) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    for predicate in predicates {
        match predicate {
            ValidityPredicate::Balance { address, .. }
            | ValidityPredicate::Nonce { address, .. } => {
                db.insert_account_info(
                    &address,
                    AccountInfo::default().with_balance(U256::from(BALANCE)).with_nonce(NONCE),
                );
            }
            ValidityPredicate::Storage { address, slot, .. } => {
                db.insert_account_info(
                    &address,
                    AccountInfo::default().with_balance(U256::from(BALANCE)).with_nonce(NONCE),
                );
                db.insert_account_storage(&address, &slot, &slot);
            }
            ValidityPredicate::BlockNumber { .. } | ValidityPredicate::FlashblockIndex { .. } => {}
        }
    }
    db
}

struct Mix {
    name: &'static str,
    timing: usize,
    state: usize,
}

impl Mix {
    fn predicates(&self) -> Vec<ValidityPredicate> {
        mixed(self.timing, self.state, 0, 0, false)
    }
}

const MIXES: &[Mix] = &[
    Mix { name: "expiry", timing: 1, state: 0 },
    Mix { name: "nonce+expiry", timing: 1, state: 1 },
    Mix { name: "3state+expiry", timing: 1, state: 3 },
    Mix { name: "6state+2timing", timing: 2, state: 6 },
    Mix { name: "14state+2timing", timing: 2, state: 14 },
    Mix { name: "62state+2timing", timing: 2, state: MAX_PREDICATES - 2 },
];

fn transfer(predicates: Vec<ValidityPredicate>) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: 8453,
        nonce: NONCE,
        gas_limit: 21_000,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 1,
        to: TxKind::Call(Address::ZERO),
        value: U256::ONE,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), address(0)),
        encoded_length,
    )
    .with_validity_predicates(predicates)
}

/// Decodes the batch from JSON the way the RPC path does, so spare `Vec` capacity left by
/// the decoder is part of what the pool ingests.
fn json_decoded(predicates: Vec<ValidityPredicate>) -> TransactionValidity {
    let json = serde_json::to_string(&TransactionValidity {
        validity: predicates,
        validity_signature: None,
    })
    .expect("serializes");
    serde_json::from_str(&json).expect("deserializes")
}

fn pooled(transaction: BasePooledTransaction) -> ValidPoolTransaction<BasePooledTransaction> {
    ValidPoolTransaction {
        transaction,
        transaction_id: TransactionId::new(0.into(), NONCE),
        propagate: false,
        timestamp: Instant::now(),
        origin: TransactionOrigin::Private,
        authority_ids: None,
    }
}

fn report_memory() {
    let plain_size = transfer(Vec::new()).size();
    println!(
        "retained memory per compiled batch (bytes); size_of::<ValidityPredicate>() = {}, size_of::<ValidityConditions>() = {}",
        core::mem::size_of::<ValidityPredicate>(),
        core::mem::size_of::<ValidityConditions>()
    );
    println!(
        "{:<16} {:>5} {:>9} {:>9} {:>6} {:>7} {:>8} {:>10} {:>8} {:>8} {:>9}",
        "mix",
        "preds",
        "heap_size",
        "total",
        "allocs",
        "alloc_B",
        "json_cap",
        "json_total",
        "tx_exact",
        "tx_json",
        "tx_growth"
    );
    for mix in MIXES {
        let exact = mix.predicates();
        let count = exact.len();
        let (conditions, allocations, allocated_bytes) =
            allocations_during(|| ValidityConditions::new(exact));
        let total = core::mem::size_of::<ValidityConditions>() + conditions.heap_size();

        let decoded = json_decoded(mix.predicates());
        let json_capacity = decoded.validity.capacity();
        let json_transaction = transfer(Vec::new()).with_validity(decoded);
        let json_total = core::mem::size_of::<ValidityConditions>()
            + json_transaction.validity_conditions().heap_size();
        let json_tx_size = json_transaction.size();
        let tx_size = transfer(mix.predicates()).size();

        let mut db = database(mix.predicates());
        let (passed, eval_allocations, _) = allocations_during(|| {
            ValidityPredicateKey::first_unsatisfied(&conditions, &mut db, &CONTEXT)
                .expect("in-memory reads cannot fail")
        });
        assert!(passed.is_none(), "{}: all predicates must pass", mix.name);
        assert_eq!(eval_allocations, 0, "{}: successful evaluation must not allocate", mix.name);
        if let Some(last) = conditions.state_predicates().last() {
            let (hit, membership_allocations, _) = allocations_during(|| conditions.contains(last));
            assert!(hit);
            assert_eq!(membership_allocations, 0, "{}: membership must not allocate", mix.name);
        }

        println!(
            "{:<16} {:>5} {:>9} {:>9} {:>6} {:>7} {:>8} {:>10} {:>8} {:>8} {:>8.1}%",
            mix.name,
            count,
            conditions.heap_size(),
            total,
            allocations,
            allocated_bytes,
            json_capacity,
            json_total,
            tx_size,
            json_tx_size,
            (json_tx_size as f64 / plain_size as f64 - 1.0) * 100.0,
        );
    }
    println!(
        "plain transfer pool size = {plain_size} B; tx_growth = accounted growth of the JSON-decoded transaction over that transfer"
    );
    println!();
}

fn ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/ingest");
    for mix in MIXES {
        group.bench_function(BenchmarkId::new("exact_capacity", mix.name), |b| {
            b.iter_batched(|| mix.predicates(), ValidityConditions::new, BatchSize::SmallInput);
        });
        group.bench_function(BenchmarkId::new("json_decoded", mix.name), |b| {
            b.iter_batched(
                || json_decoded(mix.predicates()).validity,
                ValidityConditions::new,
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// Evaluation before predicates were compiled at ingestion: scan the submitted batch in order.
fn full_scan(
    predicates: &[ValidityPredicate],
    db: &mut InMemoryDB,
    context: &PredicateContext,
) -> Option<usize> {
    predicates
        .iter()
        .position(|predicate| !predicate.matches(db, context).expect("in-memory reads cannot fail"))
}

fn evaluate(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/evaluate");
    for mix in MIXES {
        let submitted = mix.predicates();
        let conditions = ValidityConditions::new(mix.predicates());
        let mut db = database(mix.predicates());
        group.bench_function(BenchmarkId::new("pass", mix.name), |b| {
            b.iter(|| {
                let result =
                    ValidityPredicateKey::first_unsatisfied(&conditions, &mut db, &CONTEXT)
                        .expect("in-memory reads cannot fail");
                assert!(result.is_none());
            });
        });
        group.bench_function(BenchmarkId::new("timing_reject", mix.name), |b| {
            b.iter(|| {
                let result =
                    ValidityPredicateKey::first_unsatisfied(&conditions, &mut db, &EXPIRED_CONTEXT)
                        .expect("in-memory reads cannot fail");
                assert!(result.is_some());
            });
        });
        // Baseline: the pre-compilation evaluation, which scanned the submitted batch in wire
        // order and evaluated timing predicates as ordinary predicates.
        group.bench_function(BenchmarkId::new("full_scan_pass", mix.name), |b| {
            b.iter(|| {
                let result = full_scan(&submitted, &mut db, &CONTEXT);
                assert!(result.is_none());
            });
        });
        group.bench_function(BenchmarkId::new("full_scan_timing_reject", mix.name), |b| {
            b.iter(|| {
                let result = full_scan(&submitted, &mut db, &EXPIRED_CONTEXT);
                assert!(result.is_some());
            });
        });
        if mix.state > 0 {
            let failing = ValidityConditions::new(mixed(mix.timing, mix.state, 0, 0, true));
            group.bench_function(BenchmarkId::new("last_state_fails", mix.name), |b| {
                b.iter(|| {
                    let result =
                        ValidityPredicateKey::first_unsatisfied(&failing, &mut db, &CONTEXT)
                            .expect("in-memory reads cannot fail");
                    assert!(result.is_some());
                });
            });
        }
    }
    group.finish();
}

/// Evaluates every transaction of a pool-sized set once per iteration, so the working set is
/// many compiled batches rather than one hot batch. Each batch uses distinct accounts and a
/// different timing-predicate position.
fn evaluate_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/evaluate_pool");
    group.sample_size(20);
    group.throughput(Throughput::Elements(POOL_TRANSACTIONS as u64));
    for mix in MIXES.iter().filter(|mix| mix.state > 0 && mix.state <= 14) {
        let batches: Vec<Vec<ValidityPredicate>> = (0..POOL_TRANSACTIONS)
            .map(|index| mixed(mix.timing, mix.state, index * mix.state, index, false))
            .collect();
        let mut db = database(batches.iter().flatten().cloned());
        let conditions: Vec<ValidityConditions> =
            batches.into_iter().map(ValidityConditions::new).collect();
        group.bench_function(BenchmarkId::new("pass", mix.name), |b| {
            b.iter(|| {
                for conditions in &conditions {
                    let result =
                        ValidityPredicateKey::first_unsatisfied(conditions, &mut db, &CONTEXT)
                            .expect("in-memory reads cannot fail");
                    black_box(result.is_none());
                }
            });
        });
    }
    group.finish();
}

fn membership(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/contains");
    let missing = ValidityPredicate::Balance {
        address: Address::repeat_byte(0xff),
        op: ValidityOperator::Equal,
        value: U256::ZERO,
    };
    for mix in MIXES.iter().filter(|mix| mix.state > 0) {
        let conditions = ValidityConditions::new(mix.predicates());
        let last = conditions.state_predicates().last().expect("state predicate").clone();
        group.bench_function(BenchmarkId::new("hit_last_state", mix.name), |b| {
            b.iter(|| assert!(conditions.contains(black_box(&last))));
        });
        group.bench_function(BenchmarkId::new("miss", mix.name), |b| {
            b.iter(|| assert!(!conditions.contains(black_box(&missing))));
        });
    }
    group.finish();
}

/// Builder forwarding: extracting the wire sidecar from a pooled transaction.
fn forward(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/forward");
    for mix in MIXES {
        let transaction = pooled(transfer(mix.predicates()));
        group.bench_function(BenchmarkId::new("extract", mix.name), |b| {
            b.iter(|| black_box(TransactionValidity::extract(&transaction)));
        });
    }
    group.finish();
}

fn main() {
    report_memory();
    let mut criterion = Criterion::default().configure_from_args();
    ingest(&mut criterion);
    evaluate(&mut criterion);
    evaluate_pool(&mut criterion);
    membership(&mut criterion);
    forward(&mut criterion);
    criterion.final_summary();
}
