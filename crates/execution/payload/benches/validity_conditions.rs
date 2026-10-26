//! Benchmarks for compiled validity conditions (validity predicate batches): ingestion,
//! repeated evaluation in the candidate loop, resting/parking membership checks, and
//! builder forwarding, plus a retained-memory report per predicate mix.
//!
//! The memory table is printed before the timed benchmarks. It reports
//! `size_of::<ValidityConditions>() + heap_size()` next to the bytes actually requested from
//! the allocator while compiling the batch, and the pool-accounted size of a transfer carrying
//! that batch, so accounting and real allocation can be compared.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench validity_conditions
//! ```

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloy_consensus::{InMemorySize, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_payload_builder::ValidityPredicateKey;
use base_execution_txpool::{
    BasePooledTransaction, PredicateContext, TransactionValidity, ValidityConditions,
    ValidityOperator, ValidityPredicate,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use reth_primitives_traits::Recovered;
use revm::{database::InMemoryDB, state::AccountInfo};

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
            value: U256::from(if satisfied { NONCE } else { NONCE + 1 }),
        },
    }
}

/// A batch with `timing` timing predicates (an expiry first, then flashblock lower bounds in the
/// middle) and `state` state predicates rotating through storage, balance, and nonce.
///
/// State addresses start at `address_base` so pool-scale workloads can use distinct accounts.
/// When `last_fails` is set, the final state predicate is unsatisfied against [`database`].
fn mixed(
    timing: usize,
    state: usize,
    address_base: usize,
    last_fails: bool,
) -> Vec<ValidityPredicate> {
    let mut predicates = Vec::with_capacity(timing + state);
    if timing > 0 {
        predicates.push(ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(EXPIRY_BLOCK),
        });
    }
    for kind in 0..state {
        if timing > 1 && kind == state / 2 {
            for _ in 1..timing {
                predicates.push(ValidityPredicate::FlashblockIndex {
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::ONE,
                });
            }
        }
        let satisfied = !(last_fails && kind + 1 == state);
        predicates.push(state_predicate(kind, address(address_base + kind), satisfied));
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
                    address,
                    AccountInfo {
                        balance: U256::from(BALANCE),
                        nonce: NONCE,
                        ..Default::default()
                    },
                );
            }
            ValidityPredicate::Storage { address, slot, .. } => {
                db.insert_account_info(
                    address,
                    AccountInfo {
                        balance: U256::from(BALANCE),
                        nonce: NONCE,
                        ..Default::default()
                    },
                );
                db.insert_account_storage(address, slot, slot).expect("in-memory insert");
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
        mixed(self.timing, self.state, 0, false)
    }
}

const MIXES: &[Mix] = &[
    Mix { name: "expiry", timing: 1, state: 0 },
    Mix { name: "expiry+nonce", timing: 1, state: 1 },
    Mix { name: "expiry+3state", timing: 1, state: 3 },
    Mix { name: "2timing+6state", timing: 2, state: 6 },
    Mix { name: "2timing+14state", timing: 2, state: 14 },
    Mix { name: "2timing+62state", timing: 2, state: MAX_PREDICATES - 2 },
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

        // The RPC path decodes JSON without a length hint, so the vector grows by doubling and
        // may carry spare capacity into the pool.
        let json = serde_json::to_string(&TransactionValidity {
            validity: mix.predicates(),
            validity_signature: None,
        })
        .expect("serializes");
        let decoded: TransactionValidity = serde_json::from_str(&json).expect("deserializes");
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
        group.bench_function(BenchmarkId::new("spare_capacity", mix.name), |b| {
            b.iter_batched(
                || {
                    let mut predicates = Vec::with_capacity(MAX_PREDICATES);
                    predicates.extend(mix.predicates());
                    predicates
                },
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
        // Baseline: the pre-compilation evaluation, which scanned the submitted batch in order
        // and evaluated timing predicates as ordinary predicates.
        group.bench_function(BenchmarkId::new("full_scan_pass", mix.name), |b| {
            b.iter(|| {
                let result = full_scan(conditions.submitted(), &mut db, &CONTEXT);
                assert!(result.is_none());
            });
        });
        group.bench_function(BenchmarkId::new("full_scan_timing_reject", mix.name), |b| {
            b.iter(|| {
                let result = full_scan(conditions.submitted(), &mut db, &EXPIRED_CONTEXT);
                assert!(result.is_some());
            });
        });
        if mix.state > 0 {
            let failing = ValidityConditions::new(mixed(mix.timing, mix.state, 0, true));
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
/// many compiled batches rather than one hot batch.
fn evaluate_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/evaluate_pool");
    group.sample_size(20);
    group.throughput(Throughput::Elements(POOL_TRANSACTIONS as u64));
    for mix in MIXES.iter().filter(|mix| mix.state > 0 && mix.state <= 14) {
        for unique_state in [false, true] {
            let batches: Vec<Vec<ValidityPredicate>> = (0..POOL_TRANSACTIONS)
                .map(|index| {
                    mixed(
                        mix.timing,
                        mix.state,
                        if unique_state { index * mix.state } else { 0 },
                        false,
                    )
                })
                .collect();
            let mut db = database(batches.iter().flatten().cloned());
            let conditions: Vec<ValidityConditions> =
                batches.into_iter().map(ValidityConditions::new).collect();
            let name =
                format!("{}/state={}", mix.name, if unique_state { "unique" } else { "shared" });
            group.bench_function(BenchmarkId::new("pass", name), |b| {
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

fn forward(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_conditions/forward");
    for mix in MIXES {
        let conditions = ValidityConditions::new(mix.predicates());
        group.bench_function(BenchmarkId::new("to_vec", mix.name), |b| {
            b.iter(|| black_box(conditions.submitted().to_vec()));
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
