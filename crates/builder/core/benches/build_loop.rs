//! Benchmarks for the per-candidate flashblocks build loop with real EVM execution.
//!
//! Drives [`BasePayloadBuilderCtx::execute_best_transactions`] over an in-memory state.
//! Throughput is reported per considered candidate.
//!
//! - `execute_best_transactions`: one flashblock over a production-like candidate mix:
//!   successful transfers and contract calls, reverting calls, candidates rejected by the DA and
//!   gas limits, and validity-gated candidates that are parked.
//! - `mainnet_block/mode={Off,Enforce}`: a whole 11-flashblock block shaped like Base mainnet,
//!   under each resting-predicate mode. Every flashblock includes 17 transfers, rejects 10
//!   over-limit candidates, and first considers 2,840 parked validity transactions spread over
//!   518 watched addresses (median depth 2, p99 depth 46), five of which a transfer wakes.
//!
//! The observability configuration is process-global (the transaction event writer is a
//! `OnceLock`), so the arm is selected per process with `BUILD_LOOP_PROFILE`:
//!
//! - `default` (or unset): observability stays off, as in the other builder benches.
//! - `production`: enables what `bin/builder` enables in production — the global transaction
//!   event writer (`--builder.transaction-events.enabled`) appending to a temporary JSONL file,
//!   a Prometheus metrics recorder with periodic upkeep, and an `info` tracing subscriber.
//!
//! ```sh
//! BUILD_LOOP_PROFILE=default cargo bench -p base-builder-core --bench build_loop
//! BUILD_LOOP_PROFILE=production cargo bench -p base-builder-core --bench build_loop
//! ```
//!
//! To profile an arm, record the bench binary with samply:
//!
//! ```sh
//! BUILD_LOOP_PROFILE=production samply record --save-only --unstable-presymbolicate \
//!     -o prof.json.gz -- target/release/deps/build_loop-<hash> --bench <filter> --profile-time 20
//! ```

#[path = "fixtures/mainnet_block.rs"]
mod fixture;

use std::{env, hint::black_box, io, path::PathBuf, sync::Arc, thread, time::Duration};

use alloy_primitives::{Address, Bytes, U256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
    FlashblockDiagnostics, ParkableBestPayloadTransactions, RejectionCache, ResourceLimits,
    RestingPredicateMode,
};
use base_execution_txpool::{BasePooledTransaction, ValidityOperator, ValidityPredicate};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY,
    GlobalTransactionEventWriter, GlobalTransactionEventWriterInitStatus, TransactionEventProducer,
    TransactionEventWriterConfig,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use fixture::{
    MainnetBlock, OVERSIZED_CALLDATA_BYTES, Ordering, Pool, PoolTx, REJECTION_CACHE_TTL,
    SENDER_BALANCE, TX_DATA_LIMIT, address, builder_context, incompressible_calldata, parkable,
    spread_priority_fee,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use reth_revm::State;
use reth_transaction_pool::{ValidPoolTransaction, pool::PendingPool};
use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};
use tracing_subscriber::{EnvFilter, fmt};

type Candidates = BestFlashblocksTxs<
    BasePooledTransaction,
    ParkableBestPayloadTransactions<BasePooledTransaction>,
>;

const PROFILE_ENV: &str = "BUILD_LOOP_PROFILE";
const CANDIDATE_COUNT: usize = 4_000;
const BLOCK_GAS_BUDGET: u64 = 1_000_000_000;
const METRICS_UPKEEP_INTERVAL: Duration = Duration::from_secs(5);
/// How long the event writer must go without writing before it is considered drained.
const EVENT_WRITER_IDLE_POLL: Duration = Duration::from_millis(5);

/// `counter[msg.sender] += 1`: `CALLER SLOAD PUSH1 1 ADD CALLER SSTORE STOP`.
const COUNTER_CODE: [u8; 8] = [0x33, 0x54, 0x60, 0x01, 0x01, 0x33, 0x55, 0x00];
/// `REVERT(0, 0)`: `PUSH1 0 PUSH1 0 REVERT`.
const REVERT_CODE: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];

/// Observability configuration under test, selected per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// Default-off observability, matching the other builder benches.
    Default,
    /// Observability enabled as in production `bin/builder` deployments.
    Production,
}

impl Profile {
    fn from_env() -> Self {
        match env::var(PROFILE_ENV).as_deref() {
            Err(_) | Ok("") | Ok("default") => Self::Default,
            Ok("production") => Self::Production,
            Ok(other) => panic!("{PROFILE_ENV} must be `default` or `production`, got `{other}`"),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Production => "production",
        }
    }
}

/// Process-global observability installed for the production arm.
///
/// Holds the temporary event journal directory so it outlives the benchmark run.
struct Observability {
    _journal_dir: Option<tempfile::TempDir>,
    metrics: Option<PrometheusHandle>,
}

impl Observability {
    fn install(profile: Profile) -> Self {
        if profile == Profile::Default {
            return Self { _journal_dir: None, metrics: None };
        }

        let journal_dir = tempfile::tempdir().expect("create event journal dir");
        let file_path: PathBuf = journal_dir.path().join("events.jsonl");
        let status = GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
            enabled: true,
            file_path,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            required: true,
            producer: TransactionEventProducer::BaseBuilder,
            network: "bench".to_string(),
        }))
        .expect("initialize transaction event writer");
        assert_eq!(status, GlobalTransactionEventWriterInitStatus::Initialized);

        let metrics = PrometheusBuilder::new().install_recorder().expect("install recorder");
        // As in production, zero the registered metrics so the event writer counters checked
        // below are rendered before they are first incremented.
        base_metrics::initialize_registered_metrics();
        let upkeep = metrics.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(METRICS_UPKEEP_INTERVAL);
                upkeep.run_upkeep();
            }
        });

        fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(io::sink)
            .try_init()
            .expect("install tracing subscriber");

        Self { _journal_dir: Some(journal_dir), metrics: Some(metrics) }
    }

    /// Blocks until the event writer has stopped writing, so each measured iteration starts with
    /// an empty queue, as it would at production candidate rates.
    fn wait_for_event_writer(&self) {
        let Some(metrics) = &self.metrics else { return };
        let bytes_written = || {
            metrics
                .render()
                .lines()
                .find(|line| line.starts_with("transaction_events_bytes_written"))
                .and_then(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
                .expect("transaction_events_bytes_written is rendered")
        };
        let mut last = bytes_written();
        loop {
            thread::sleep(EVENT_WRITER_IDLE_POLL);
            let current = bytes_written();
            if current == last {
                return;
            }
            last = current;
        }
    }

    /// Prints the event writer counters and fails if any event was dropped or none was written.
    ///
    /// A full writer queue drops events before the producer does its usual work, so a run with
    /// drops would measure a cheaper loop than production. Missing counters fail the check rather
    /// than passing it vacuously.
    fn check_event_writer(&self) {
        let Some(metrics) = &self.metrics else { return };
        let mut dropped_lines = 0;
        let mut bytes_written = None;
        for line in metrics.render().lines() {
            if line.starts_with('#') || !line.contains("transaction_events_") {
                continue;
            }
            eprintln!("{line}");
            let value = line.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok());
            if line.contains("dropped_events") {
                dropped_lines += 1;
                assert_eq!(value, Some(0.0), "event writer dropped events: {line}");
            } else if line.starts_with("transaction_events_bytes_written") {
                bytes_written = value;
            }
        }
        assert!(dropped_lines > 0, "transaction_events_dropped_events is not rendered");
        assert!(
            bytes_written.is_some_and(|bytes| bytes > 0.0),
            "event writer wrote no bytes: {bytes_written:?}",
        );
    }
}

/// Role a candidate plays in the build loop.
#[derive(Debug, Clone, Copy)]
enum Candidate {
    /// Plain ETH transfer to a fresh account.
    Transfer,
    /// Call that increments a per-sender storage counter.
    CounterCall,
    /// Call that reverts.
    RevertingCall,
    /// Calldata whose DA size exceeds the per-transaction DA limit.
    OverDaLimit,
    /// Gas limit above the remaining block gas budget.
    OverGasLimit,
    /// Validity predicate that is never satisfied, so the candidate is parked.
    Parked,
}

impl Candidate {
    /// Of every 20 candidates: 17 included (one of them reverting), one over the DA limit, one
    /// over the gas limit, and one parked.
    const fn for_index(index: usize) -> Self {
        match index % 20 {
            0 => Self::OverDaLimit,
            1 => Self::OverGasLimit,
            2 => Self::Parked,
            3 => Self::RevertingCall,
            4..=11 => Self::CounterCall,
            _ => Self::Transfer,
        }
    }
}

fn sender(index: usize) -> Address {
    address(1, index)
}

fn counter_contract() -> Address {
    address(2, 0)
}

fn revert_contract() -> Address {
    address(2, 1)
}

fn pooled_candidate(index: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let candidate = Candidate::for_index(index);
    let (to, gas_limit, input) = match candidate {
        Candidate::Transfer | Candidate::Parked => (address(3, index), 21_000, Bytes::new()),
        Candidate::CounterCall => (counter_contract(), 100_000, Bytes::new()),
        Candidate::RevertingCall => (revert_contract(), 100_000, Bytes::new()),
        Candidate::OverDaLimit => {
            (address(3, index), 200_000, incompressible_calldata(index, OVERSIZED_CALLDATA_BYTES))
        }
        Candidate::OverGasLimit => (address(3, index), BLOCK_GAS_BUDGET + 1, Bytes::new()),
    };
    let predicates = if matches!(candidate, Candidate::Parked) {
        vec![ValidityPredicate::Balance {
            address: address(4, index),
            op: ValidityOperator::Equal,
            value: U256::ONE,
        }]
    } else {
        Vec::new()
    };
    PoolTx {
        sender_id: index,
        sender: sender(index),
        to,
        gas_limit,
        priority_fee: spread_priority_fee(0, index),
        value: U256::from(1 + index),
        input,
        predicates,
    }
    .into_pooled()
}

/// One flashblock build: the context, the funded pre-state, and the pending pool.
struct Fixture {
    ctx: BasePayloadBuilderCtx,
    pre_state: InMemoryDB,
    pool: Pool,
    limits: ResourceLimits,
}

impl Fixture {
    fn new() -> Self {
        let ctx = builder_context();

        let mut pre_state = InMemoryDB::default();
        let balance = U256::from(SENDER_BALANCE);
        for index in 0..CANDIDATE_COUNT {
            pre_state.insert_account_info(sender(index), AccountInfo::from_balance(balance));
        }
        for (contract, code) in
            [(counter_contract(), &COUNTER_CODE[..]), (revert_contract(), &REVERT_CODE[..])]
        {
            pre_state.insert_account_info(
                contract,
                AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(code))),
            );
        }

        let mut pool = PendingPool::new(Ordering::coinbase_tip());
        for index in 0..CANDIDATE_COUNT {
            pool.add_transaction(pooled_candidate(index), 0);
        }

        let limits = ResourceLimits {
            block_gas_limit: BLOCK_GAS_BUDGET,
            tx_data_limit: Some(TX_DATA_LIMIT),
            ..Default::default()
        };

        Self { ctx, pre_state, pool, limits }
    }

    /// Wraps the pool iterator the way `payload.rs` does. Each iteration gets a fresh rejection
    /// cache so every iteration considers the same candidates.
    fn best_transactions(&self) -> Candidates {
        BestFlashblocksTxs::new(
            parkable(&self.pool),
            RejectionCache::new(CANDIDATE_COUNT as u64, REJECTION_CACHE_TTL),
        )
    }

    fn fresh_state(&self) -> State<InMemoryDB> {
        State::builder().with_database(self.pre_state.clone()).with_bundle_update().build()
    }

    fn build(&self, state: &mut State<InMemoryDB>, best: &mut Candidates) -> FlashblockDiagnostics {
        let mut info = ExecutionInfo::default();
        let mut deferrals = BlockDeferrals::default();
        self.ctx
            .execute_best_transactions(&mut info, &mut deferrals, state, best, &self.limits)
            .expect("build loop succeeds")
    }
}

fn build_loop_bench(c: &mut Criterion, profile: Profile, observability: &Observability) {
    let fixture = Fixture::new();

    let diag = fixture.build(&mut fixture.fresh_state(), &mut fixture.best_transactions());
    eprintln!(
        "build_loop[{}]: considered={} included={} deferred={} rejected_gas={} rejected_da={} rejected_other={}",
        profile.label(),
        diag.txs_considered,
        diag.txs_included,
        diag.txs_deferred,
        diag.txs_rejected_gas,
        diag.txs_rejected_da,
        diag.txs_rejected_other,
    );
    assert_eq!(diag.txs_considered, CANDIDATE_COUNT as u64, "every candidate is considered");
    let per_slot = CANDIDATE_COUNT as u64 / 20;
    assert_eq!(
        (
            diag.txs_included,
            diag.txs_deferred,
            diag.txs_rejected_gas,
            diag.txs_rejected_da,
            diag.txs_rejected_other,
        ),
        (17 * per_slot, per_slot, per_slot, per_slot, 0),
        "candidate outcomes match the documented mix",
    );

    let mut group = c.benchmark_group("build_loop");
    group.sample_size(20);
    group.throughput(Throughput::Elements(diag.txs_considered));
    group.bench_function(BenchmarkId::new("execute_best_transactions", profile.label()), |b| {
        // `iter_batched_ref` drops the state and iterator after timing stops.
        b.iter_batched_ref(
            || {
                observability.wait_for_event_writer();
                (fixture.fresh_state(), fixture.best_transactions())
            },
            |(state, best)| black_box(fixture.build(state, best)),
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn mainnet_block_bench(c: &mut Criterion, profile: Profile, observability: &Observability) {
    let block = MainnetBlock::new();

    let mut group = c.benchmark_group("build_loop");
    group.sample_size(10);
    for mode in [RestingPredicateMode::Off, RestingPredicateMode::Enforce] {
        let expected = block.expected_totals(mode);
        let totals = block.build(&mut block.fresh_state(), &block.rejection_cache(), mode);
        eprintln!(
            "build_loop/mainnet_block[{} mode={mode:?}]: considered={} included={} deferred={} rejected_gas={} rejected_da={} rejected_other={}",
            profile.label(),
            totals.considered,
            totals.included,
            totals.deferred,
            totals.rejected_gas,
            totals.rejected_da,
            totals.rejected_other,
        );
        assert_eq!(totals, expected, "block outcomes match the documented workload");

        group.throughput(Throughput::Elements(totals.considered));
        group.bench_function(
            BenchmarkId::new(format!("mainnet_block/mode={mode:?}"), profile.label()),
            |b| {
                // `iter_batched_ref` drops the state and cache after timing stops.
                b.iter_batched_ref(
                    || {
                        observability.wait_for_event_writer();
                        (block.fresh_state(), block.rejection_cache())
                    },
                    |(state, rejection_cache)| {
                        let totals = block.build(state, rejection_cache, mode);
                        assert_eq!(
                            totals, expected,
                            "block outcomes match the documented workload"
                        );
                        black_box(totals)
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

fn main() {
    let profile = Profile::from_env();
    let observability = Observability::install(profile);

    let mut criterion = Criterion::default().configure_from_args();
    build_loop_bench(&mut criterion, profile, &observability);
    mainnet_block_bench(&mut criterion, profile, &observability);
    criterion.final_summary();
    observability.check_event_writer();
}
