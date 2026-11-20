//! Same-block inclusion and predicate-evaluation cost of validity transactions with alternative
//! (`anyOf`) conditions, compared with the flat submissions they replace, on the native
//! (post-Denim) builder's selection loop with the default 10 ms predicate budget.
//!
//! An opportunity is one intent, "land once if any of these routes holds", submitted either as
//! one nested transaction or as one flat transaction per route. Each measured pair builds two
//! blocks from the same parent state: a parking block in which every waiter arrives fresh and
//! stays unsatisfied, then a wake block in which transactions bidding below the waiters write
//! watched storage slots. Every write wakes every waiter; [`ELIGIBLE`] opportunities come into
//! range on the last write. An opportunity filled by more than one of its flat transactions
//! counts once, and a waiter included outside the eligible set fails the run.
//!
//! Two workloads are measured, both admissible under `--validity-max-predicates 16`:
//!
//! - Customer routes: a shared trigger and two routes with two and three pool-price legs, each
//!   leg bounded below and above (two flat transactions of 6 and 8 predicates, or one nested
//!   transaction of 14). The wake block moves the trigger; route A, route B, or both are in range.
//! - Wider alternatives: 2 or 5 routes of `expiry AND lo <= slot <= hi` (6 or 15 predicates), and
//!   8 routes of `expiry AND slot >= lo` (16), each route on its own slot. The wake block writes
//!   the last slot only, or every slot.
//!
//! Predicate evaluation time, budget cutoffs, and evaluated or deferred candidates are read from
//! the builder's own metrics. State is in memory and no state root is computed, so absolute times
//! are lower than on a running node.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_validity_alternatives
//! ```
//!
//! `VALIDITY_ALTERNATIVES_BENCH_PAIRS` overrides the measured block pairs per row (default 200),
//! and `VALIDITY_ALTERNATIVES_BENCH_LOADS` the comma-separated opportunity counts (default
//! 512,2048,4096).

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
    ParkableBestPayloadTransactions, RestingPredicates, SharedRestingPredicates,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, TransactionValidity,
    ValidityExpression, ValidityOperator, ValidityPredicate,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::db::State;
use reth_transaction_pool::{
    TransactionOrigin, ValidPoolTransaction, identifier::TransactionId, pool::PendingPool,
};
use revm::{
    Database,
    database::{InMemoryDB, TransitionState},
    state::{AccountInfo, Bytecode},
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;
type Pooled = Arc<ValidPoolTransaction<BasePooledTransaction>>;
/// Alternative conjunctions of one transaction; a flat transaction has exactly one.
type Groups = Vec<Vec<ValidityPredicate>>;

const CHAIN_ID: u64 = 8_453;
const DENIM_TIMESTAMP: u64 = 1;
const BLOCK_GAS_LIMIT: u64 = 60_000_000;
const TRANSFER_GAS: u64 = 21_000;
const WRITE_GAS: u64 = 100_000;
/// Plain transfers in every block, bidding below every other transaction.
const FILLER: usize = 120;
/// Opportunities that come into range in each wake block.
const ELIGIBLE: usize = 16;
/// Mainnet admission limit on normalized predicate occurrences per transaction.
const MAX_PREDICATES: usize = 16;
const WARMUP_PAIRS: usize = 5;
const DEFAULT_PAIRS: usize = 200;
const DEFAULT_LOADS: [usize; 3] = [512, 2_048, 4_096];
/// Value every write stores and every in-range leg holds: within `[LOWER, IN_RANGE_UPPER]`, above
/// `OUT_OF_RANGE_UPPER`.
const WRITTEN: u64 = 15;
const LOWER: u64 = 10;
const IN_RANGE_UPPER: u64 = 20;
const OUT_OF_RANGE_UPPER: u64 = 12;
/// Lower bound a written slot never reaches, for threshold routes that stay unsatisfied.
const UNREACHED_LOWER: u64 = 100;
const WAITER_TIP: u128 = 10;
const FILLER_TIP: u128 = 1;
/// Contract whose only code is `SSTORE(calldata[0..32], calldata[32..64])`.
const WATCHED: Address = Address::repeat_byte(0xc0);
/// `PUSH1 0x20 CALLDATALOAD PUSH0 CALLDATALOAD SSTORE STOP`.
const WATCHED_CODE: [u8; 7] = [0x60, 0x20, 0x35, 0x5f, 0x35, 0x55, 0x00];
const ROUTE_A: &[usize] = &[1, 2];
const ROUTE_B: &[usize] = &[3, 4, 5];
const TRIGGER_SLOT: usize = 0;

fn address(index: usize) -> Address {
    Address::from_word(keccak256(index.to_be_bytes()))
}

fn storage(slot: usize, op: ValidityOperator, value: u64) -> ValidityPredicate {
    ValidityPredicate::Storage {
        address: WATCHED,
        slot: U256::from(slot),
        mask: U256::MAX,
        op,
        value: U256::from(value),
    }
}

fn expiry() -> ValidityPredicate {
    ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(1_000),
    }
}

/// `lo <= slot <= hi`, with `hi` below [`WRITTEN`] unless `in_range`.
fn range(slot: usize, in_range: bool) -> [ValidityPredicate; 2] {
    let upper = if in_range { IN_RANGE_UPPER } else { OUT_OF_RANGE_UPPER };
    [
        storage(slot, ValidityOperator::GreaterThanOrEqual, LOWER),
        storage(slot, ValidityOperator::LessThanOrEqual, upper),
    ]
}

/// An executable transaction; `groups` attaches validity conditions, nested when it has more
/// than one group.
fn transaction(
    sender_index: usize,
    priority_fee: u128,
    to: Address,
    gas_limit: u64,
    input: Bytes,
    mut groups: Groups,
) -> Pooled {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit,
        max_fee_per_gas: 1_000_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(to),
        value: U256::from(sender_index),
        access_list: Default::default(),
        input,
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), address(sender_index)),
        encoded_length,
    );
    let validity = match groups.len() {
        0 => None,
        1 => Some(ValidityExpression::All(groups.pop().expect("one group"))),
        _ => Some(ValidityExpression::AnyOf(groups)),
    };
    let transaction = match validity {
        Some(validity) => {
            validity
                .validate(MAX_PREDICATES)
                .expect("every workload is admissible at the mainnet predicate limit");
            transaction.with_validity(TransactionValidity { validity, validity_signature: None })
        }
        None => transaction,
    };
    // Pool admission computes the hash and the DA-size estimate, so transactions reach the
    // builder with both cached.
    reth_transaction_pool::PoolTransaction::hash(&transaction);
    transaction.estimated_compressed_size();
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new((sender_index as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// One row group of the table: a set of opportunities, the slots that already hold [`WRITTEN`],
/// and the wake blocks to measure after parking.
struct Scenario {
    workload: String,
    arm: &'static str,
    /// Transactions submitted per opportunity, each given by its alternative groups.
    opportunities: Vec<Vec<Groups>>,
    preset: Vec<usize>,
    /// Labelled wake blocks, each writing its slots in order.
    wakes: Vec<(String, Vec<usize>)>,
}

impl Scenario {
    /// Splits each opportunity's routes into one flat transaction per route, or one nested one.
    fn submissions(routes: Groups, nested: bool) -> Vec<Groups> {
        if nested { vec![routes] } else { routes.into_iter().map(|route| vec![route]).collect() }
    }

    const fn arm(nested: bool) -> &'static str {
        if nested { "nested" } else { "flat" }
    }

    /// Shared trigger plus route A, route B, or both in range for the eligible opportunities.
    fn customer(case: Case, opportunities: usize, nested: bool) -> Self {
        let route = |legs: &[usize], in_range: bool| {
            let mut group =
                vec![expiry(), storage(TRIGGER_SLOT, ValidityOperator::GreaterThanOrEqual, LOWER)];
            // A route out of range fails only on its last leg, after reading every other leg.
            for (position, leg) in legs.iter().enumerate() {
                group.extend(range(*leg, in_range || position + 1 < legs.len()));
            }
            group
        };
        let opportunities = (0..opportunities)
            .map(|opportunity| {
                let eligible = opportunity < ELIGIBLE;
                let routes = vec![
                    route(ROUTE_A, eligible && !matches!(case, Case::BOnly)),
                    route(ROUTE_B, eligible && !matches!(case, Case::AOnly)),
                ];
                Self::submissions(routes, nested)
            })
            .collect();
        Self {
            workload: format!("customer routes, {} eligible", case.label()),
            arm: Self::arm(nested),
            opportunities,
            preset: ROUTE_A.iter().chain(ROUTE_B).copied().collect(),
            wakes: vec![("wake, trigger moves".to_owned(), vec![TRIGGER_SLOT])],
        }
    }

    /// `routes` alternatives on their own slots; only the last route of an eligible opportunity
    /// comes into range.
    fn wider(shape: Shape, routes: usize, opportunities: usize, nested: bool) -> Self {
        let opportunities = (0..opportunities)
            .map(|opportunity| {
                let groups = (0..routes)
                    .map(|slot| {
                        let in_range = opportunity < ELIGIBLE && slot + 1 == routes;
                        let mut group = vec![expiry()];
                        match shape {
                            Shape::Range => group.extend(range(slot, in_range)),
                            Shape::Threshold => group.push(storage(
                                slot,
                                ValidityOperator::GreaterThanOrEqual,
                                if in_range { LOWER } else { UNREACHED_LOWER },
                            )),
                        }
                        group
                    })
                    .collect();
                Self::submissions(groups, nested)
            })
            .collect();
        Self {
            workload: format!("{routes} {} routes", shape.label()),
            arm: Self::arm(nested),
            opportunities,
            preset: Vec::new(),
            wakes: vec![
                ("wake, last slot written".to_owned(), vec![routes - 1]),
                (format!("wake, all {routes} slots written"), (0..routes).collect()),
            ],
        }
    }
}

#[derive(Clone, Copy)]
enum Case {
    AOnly,
    BOnly,
    Both,
}

impl Case {
    const fn label(self) -> &'static str {
        match self {
            Self::AOnly => "A only",
            Self::BOnly => "B only",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// `expiry AND lo <= slot <= hi`: a write can wake a waiter without satisfying it.
    Range,
    /// `expiry AND slot >= lo`: the most alternatives the predicate limit admits.
    Threshold,
}

impl Shape {
    const fn label(self) -> &'static str {
        match self {
            Self::Range => "range",
            Self::Threshold => "threshold",
        }
    }
}

/// Pools and parent state built from a [`Scenario`].
struct Workload {
    db: InMemoryDB,
    parking: Pool,
    wakes: Vec<(String, Pool, usize)>,
    /// Senders of the transactions that can fill each eligible opportunity.
    eligible: Vec<Vec<usize>>,
}

impl Workload {
    fn new(scenario: &Scenario) -> Self {
        let mut waiters = Vec::new();
        let mut eligible = Vec::new();
        for (opportunity, submissions) in scenario.opportunities.iter().enumerate() {
            let first = waiters.len();
            for groups in submissions {
                waiters.push(transaction(
                    waiters.len(),
                    WAITER_TIP,
                    Address::repeat_byte(0xee),
                    TRANSFER_GAS,
                    Bytes::new(),
                    groups.clone(),
                ));
            }
            if opportunity < ELIGIBLE {
                eligible.push((first..waiters.len()).collect());
            }
        }

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            WATCHED,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&WATCHED_CODE))),
        );
        for slot in &scenario.preset {
            db.insert_account_storage(WATCHED, U256::from(*slot), U256::from(WRITTEN))
                .expect("in-memory insert cannot fail");
        }
        let max_writes = scenario.wakes.iter().map(|(_, slots)| slots.len()).max().unwrap_or(0);
        let first_writer = waiters.len();
        let first_filler = first_writer + max_writes;
        for sender in 0..first_filler + FILLER {
            db.insert_account_info(
                address(sender),
                AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
            );
        }

        let pool = |slots: &[usize]| {
            let mut pool = PendingPool::new(Ordering::coinbase_tip());
            for waiter in &waiters {
                pool.add_transaction(Arc::clone(waiter), 0);
            }
            // Writes bid below the waiters, in slot order, so the waiters each write wakes are
            // re-evaluated before the next write commits.
            for (position, slot) in slots.iter().enumerate() {
                let mut input = [0u8; 64];
                input[..32].copy_from_slice(&U256::from(*slot).to_be_bytes::<32>());
                input[32..].copy_from_slice(&U256::from(WRITTEN).to_be_bytes::<32>());
                pool.add_transaction(
                    transaction(
                        first_writer + position,
                        WAITER_TIP - 1 - position as u128,
                        WATCHED,
                        WRITE_GAS,
                        Bytes::copy_from_slice(&input),
                        Vec::new(),
                    ),
                    0,
                );
            }
            for filler in 0..FILLER {
                pool.add_transaction(
                    transaction(
                        first_filler + filler,
                        FILLER_TIP,
                        Address::repeat_byte(0xee),
                        TRANSFER_GAS,
                        Bytes::new(),
                        Vec::new(),
                    ),
                    0,
                );
            }
            pool
        };
        let wakes = scenario
            .wakes
            .iter()
            .map(|(label, slots)| (label.clone(), pool(slots), slots.len()))
            .collect();
        Self { db, parking: pool(&[]), wakes, eligible }
    }
}

fn payload_context() -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        gas_limit: BLOCK_GAS_LIMIT,
        ..Default::default()
    }));
    let payload_id = PayloadId::new([0; 8]);
    let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
        payload_attributes: EthPayloadBuilderAttributes {
            id: payload_id,
            parent: parent.hash(),
            timestamp: DENIM_TIMESTAMP,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        gas_limit: Some(BLOCK_GAS_LIMIT),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig::default(),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

/// Cumulative builder metrics scraped from the Prometheus recorder.
#[derive(Clone, Copy, Default)]
struct Counters {
    eval_seconds: f64,
    cutoff_builds: f64,
    evaluated: f64,
    deferred: f64,
}

impl Counters {
    fn read(handle: &PrometheusHandle) -> Self {
        let rendered = handle.render();
        let value = |series: &str| {
            rendered
                .lines()
                .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let outcome = |outcome: &str| {
            value(&format!(
                "base_builder_validity_predicate_evaluations_total{{outcome=\"{outcome}\"}}"
            ))
        };
        Self {
            eval_seconds: value("base_builder_validity_predicate_eval_duration_per_block_sum"),
            cutoff_builds: value("base_builder_validity_predicate_eval_cutoff_builds_total"),
            evaluated: outcome("matched")
                + outcome("not_satisfied")
                + outcome("rescan_matched")
                + outcome("rescan_not_satisfied"),
            deferred: outcome("budget_exhausted") + outcome("rescan_budget_exhausted"),
        }
    }
}

#[derive(Default)]
struct BlockStats {
    selection_ms: Vec<f64>,
    eval_ms: Vec<f64>,
    cutoff_builds: f64,
    evaluated: f64,
    deferred: f64,
    filled: usize,
}

impl BlockStats {
    fn quantile(samples: &[f64], quantile: f64) -> f64 {
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted[((sorted.len() - 1) as f64 * quantile).round() as usize]
    }

    fn record(&mut self, handle: &PrometheusHandle, run: impl FnOnce() -> (Duration, usize)) {
        let before = Counters::read(handle);
        let (elapsed, filled) = run();
        let after = Counters::read(handle);
        self.selection_ms.push(elapsed.as_secs_f64() * 1e3);
        self.eval_ms.push((after.eval_seconds - before.eval_seconds) * 1e3);
        self.cutoff_builds += after.cutoff_builds - before.cutoff_builds;
        self.evaluated += after.evaluated - before.evaluated;
        self.deferred += after.deferred - before.deferred;
        self.filled += filled;
    }

    fn row(&self, out: &mut String, label: &str, block: &str, eligible: usize) {
        let blocks = self.selection_ms.len();
        writeln!(
            out,
            "| {label} | {block} | {}/{} | {:.0}/{blocks} | {:.0} | {:.0} | {:.2} / {:.2} | {:.2} / {:.2} |",
            self.filled,
            eligible * blocks,
            self.cutoff_builds,
            self.evaluated / blocks as f64,
            self.deferred / blocks as f64,
            Self::quantile(&self.eval_ms, 0.5),
            Self::quantile(&self.eval_ms, 0.99),
            Self::quantile(&self.selection_ms, 0.5),
            Self::quantile(&self.selection_ms, 0.99),
        )
        .expect("writing to a string cannot fail");
    }
}

/// Runs the selection loop for one block and returns its duration and the eligible opportunities
/// it filled. Panics if a write or filler is dropped or a waiter outside the eligible set lands.
fn run_block(
    ctx: &Ctx,
    pool: &Pool,
    workload: &Workload,
    resting: &SharedRestingPredicates,
    ordinary: usize,
) -> (Duration, usize) {
    let mut state =
        State::builder().with_database(workload.db.clone()).with_bundle_update().build();
    state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
    let mut builder = ctx.block_builder(&mut state).expect("block builder");
    let mut info = ExecutionInfo::default();
    let start = Instant::now();
    let best = ParkableBestPayloadTransactions::new(Box::new(
        ParkedBestTransactions::new(pool.best(), Ordering::coinbase_tip(), 0)
            .with_hold(Some(resting.iterator_hold())),
    ));
    ctx.execute_best_transactions(&mut info, &mut builder, best, Some(resting))
        .expect("in-memory execution cannot fail");
    let elapsed = start.elapsed();
    let receipts = builder.executor().receipts().len();
    drop(builder);
    assert!(receipts >= ordinary, "every write and filler transaction must be included");

    let mut included = 0;
    let mut filled = 0;
    for senders in &workload.eligible {
        let landed = senders
            .iter()
            .filter(|sender| {
                state
                    .basic(address(**sender))
                    .expect("in-memory read cannot fail")
                    .is_some_and(|account| account.nonce == 1)
            })
            .count();
        included += landed;
        filled += usize::from(landed > 0);
    }
    assert_eq!(receipts - ordinary, included, "only eligible waiters may be included");
    (elapsed, filled)
}

/// Measures a parking block and then a wake block from the same parent, `pairs` times.
fn measure(
    ctx: &Ctx,
    handle: &PrometheusHandle,
    workload: &Workload,
    wake: &Pool,
    writes: usize,
    pairs: usize,
) -> (BlockStats, BlockStats) {
    let (mut parking, mut waking) = (BlockStats::default(), BlockStats::default());
    for pair in 0..WARMUP_PAIRS + pairs {
        let (mut warmup_parking, mut warmup_waking) =
            (BlockStats::default(), BlockStats::default());
        let (parking, waking) = if pair < WARMUP_PAIRS {
            (&mut warmup_parking, &mut warmup_waking)
        } else {
            (&mut parking, &mut waking)
        };
        let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
        resting.begin_block(B256::ZERO, 1, Some(&TransitionState::default()));
        let shared = SharedRestingPredicates::new(resting);
        parking.record(handle, || run_block(ctx, &workload.parking, workload, &shared, FILLER));

        let mut resting = shared.take_predicates();
        let parent = B256::from(U256::from(1));
        resting.end_block(parent);
        resting.begin_block(parent, 2, Some(&TransitionState::default()));
        let shared = SharedRestingPredicates::new(resting);
        waking.record(handle, || run_block(ctx, wake, workload, &shared, FILLER + writes));
    }
    (parking, waking)
}

fn env_or<T>(name: &str, default: T, parse: impl FnOnce(&str) -> T) -> T {
    std::env::var(name).map_or(default, |value| parse(&value))
}

fn main() {
    let pairs = env_or("VALIDITY_ALTERNATIVES_BENCH_PAIRS", DEFAULT_PAIRS, |pairs| {
        pairs.parse().expect("VALIDITY_ALTERNATIVES_BENCH_PAIRS is a number")
    });
    let loads = env_or("VALIDITY_ALTERNATIVES_BENCH_LOADS", DEFAULT_LOADS.to_vec(), |loads| {
        loads
            .split(',')
            .map(|load| load.trim().parse().expect("VALIDITY_ALTERNATIVES_BENCH_LOADS are numbers"))
            .collect()
    });

    // Transaction events are on in production and cost time in the selection loop.
    let events = tempfile::tempdir().expect("temporary event directory");
    GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
        enabled: true,
        ..TransactionEventWriterConfig::disabled(
            TransactionEventProducer::BaseBuilder,
            "bench",
            events.path().join("events.jsonl"),
        )
    }))
    .expect("event writer initializes");
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let ctx = payload_context();

    let mut out = format!(
        "pairs={pairs}\n\n| Workload | Arm | Opportunities | Block | Same-block fills | \
         Cutoff builds | Evaluated/block | Deferred/block | Predicate eval p50 / p99 ms | \
         Selection loop p50 / p99 ms |\n|---|---|---:|---|---:|---:|---:|---:|---:|---:|\n"
    );
    metrics::with_local_recorder(&recorder, || {
        for &opportunities in &loads {
            let mut scenarios = Vec::new();
            for case in [Case::AOnly, Case::BOnly, Case::Both] {
                for nested in [false, true] {
                    scenarios.push(Scenario::customer(case, opportunities, nested));
                }
            }
            for (shape, routes) in [(Shape::Range, 2), (Shape::Range, 5), (Shape::Threshold, 8)] {
                for nested in [false, true] {
                    scenarios.push(Scenario::wider(shape, routes, opportunities, nested));
                }
            }
            for scenario in &scenarios {
                let workload = Workload::new(scenario);
                let label = format!("{} | {} | {opportunities}", scenario.workload, scenario.arm);
                for (index, (block, wake, writes)) in workload.wakes.iter().enumerate() {
                    let (parking, waking) = measure(&ctx, &handle, &workload, wake, *writes, pairs);
                    // Parking blocks are identical across wake blocks; report the first.
                    if index == 0 {
                        parking.row(&mut out, &label, "parking", 0);
                    }
                    waking.row(&mut out, &label, block, ELIGIBLE);
                }
                eprintln!("measured {label}");
            }
        }
    });
    println!("{out}");
}
