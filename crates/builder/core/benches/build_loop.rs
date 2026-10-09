//! Benchmarks for the per-candidate flashblocks build loop with real EVM execution.
//!
//! Drives [`BasePayloadBuilderCtx::execute_best_transactions`] over an in-memory state with a
//! production-like candidate mix: successful transfers and contract calls, reverting calls,
//! candidates rejected by the DA and gas limits, and validity-gated candidates that are parked.
//! Throughput is reported per considered candidate.
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
//! To profile an arm, record the bench binary with samply and summarize the hot path:
//!
//! ```sh
//! BUILD_LOOP_PROFILE=production samply record --save-only --unstable-presymbolicate \
//!     -o prof.json.gz -- target/release/deps/build_loop-<hash> --bench --profile-time 20
//! etc/scripts/perf/summarize_profile.py prof.json.gz --root execute_best_transactions
//! ```

use std::{
    env,
    hint::black_box,
    io,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256, keccak256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
    FlashblockDiagnostics, ParkableBestPayloadTransactions, RejectionCache, ResourceLimits,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use base_observability_events::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY,
    GlobalTransactionEventWriter, GlobalTransactionEventWriterInitStatus, TransactionEventProducer,
    TransactionEventWriterConfig,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use reth_chainspec::ChainSpec;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::State;
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};
use tracing_subscriber::{EnvFilter, fmt};

type Candidates = BestFlashblocksTxs<
    BasePooledTransaction,
    ParkableBestPayloadTransactions<BasePooledTransaction>,
>;
type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;

const PROFILE_ENV: &str = "BUILD_LOOP_PROFILE";
const CHAIN_ID: u64 = 901;
const CANDIDATE_COUNT: usize = 4_000;
const PARENT_GAS_LIMIT: u64 = 30_000_000;
const PARENT_BASE_FEE: u64 = 1_000_000_000;
const MAX_FEE_PER_GAS: u128 = 10_000_000_000;
const BLOCK_GAS_BUDGET: u64 = 1_000_000_000;
const TX_DATA_LIMIT: u64 = 1_000;
const OVERSIZED_CALLDATA_BYTES: usize = 4_096;
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

    /// Prints the event writer counters and fails if any event was dropped.
    ///
    /// A full writer queue drops events before the producer does its usual work, so a run with
    /// drops would measure a cheaper loop than production.
    fn check_event_writer(&self) {
        let Some(metrics) = &self.metrics else { return };
        for line in metrics.render().lines() {
            if line.starts_with('#') || !line.contains("transaction_events_") {
                continue;
            }
            eprintln!("{line}");
            if line.contains("dropped_events") {
                let count = line.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok());
                assert_eq!(count, Some(0.0), "event writer dropped events: {line}");
            }
        }
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

/// Deterministic address in a numbered namespace.
fn address(namespace: u64, index: usize) -> Address {
    Address::from_word(keccak256([namespace.to_be_bytes(), (index as u64).to_be_bytes()].concat()))
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

/// Incompressible calldata so the estimated DA size stays above [`TX_DATA_LIMIT`].
fn oversized_calldata(index: usize) -> Bytes {
    (0..OVERSIZED_CALLDATA_BYTES / 32)
        .flat_map(|chunk| keccak256([index.to_be_bytes(), chunk.to_be_bytes()].concat()).0)
        .collect()
}

fn pooled_candidate(index: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let candidate = Candidate::for_index(index);
    let (to, gas_limit, input) = match candidate {
        Candidate::Transfer | Candidate::Parked => (address(3, index), 21_000, Bytes::new()),
        Candidate::CounterCall => (counter_contract(), 100_000, Bytes::new()),
        Candidate::RevertingCall => (revert_contract(), 100_000, Bytes::new()),
        Candidate::OverDaLimit => (address(3, index), 200_000, oversized_calldata(index)),
        Candidate::OverGasLimit => (address(3, index), BLOCK_GAS_BUDGET + 1, Bytes::new()),
    };
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        max_priority_fee_per_gas: 1_000_000 * (1 + (index as u128 * 7_919) % 1_000),
        to: TxKind::Call(to),
        value: U256::from(1 + index),
        access_list: Default::default(),
        input,
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let mut transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender(index)),
        encoded_length,
    );
    if matches!(candidate, Candidate::Parked) {
        transaction = transaction.with_validity_predicates(vec![ValidityPredicate::Balance {
            address: address(4, index),
            op: ValidityOperator::Equal,
            value: U256::ONE,
        }]);
    }

    // Pool validation computes the DA size before a transaction reaches the pool, so the builder
    // reads the cached value. Prime it here to match.
    transaction.estimated_compressed_size();

    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new((index as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
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
        let genesis = serde_json::from_value(serde_json::json!({
            "config": { "chainId": CHAIN_ID },
            "gasLimit": format!("{PARENT_GAS_LIMIT:#x}"),
            "timestamp": "0x0"
        }))
        .expect("valid genesis");
        let chain_spec = Arc::new(BaseChainSpec::from(
            ChainSpec::builder().chain(CHAIN_ID.into()).genesis(genesis).cancun_activated().build(),
        ));
        let parent = Arc::new(SealedHeader::seal_slow(Header {
            gas_limit: PARENT_GAS_LIMIT,
            base_fee_per_gas: Some(PARENT_BASE_FEE),
            ..Default::default()
        }));
        let ctx = BasePayloadBuilderCtx::for_test(chain_spec, parent);

        let mut pre_state = InMemoryDB::default();
        let balance = U256::from(10u128.pow(24));
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
        let mut best = self.pool.best();
        best.no_updates();
        BestFlashblocksTxs::new(
            ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
                best,
                Ordering::coinbase_tip(),
                0,
            ))),
            RejectionCache::new(CANDIDATE_COUNT as u64, Duration::from_secs(60)),
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
        b.iter_batched(
            || {
                observability.wait_for_event_writer();
                (fixture.fresh_state(), fixture.best_transactions())
            },
            |(mut state, mut best)| black_box(fixture.build(&mut state, &mut best)),
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn main() {
    let profile = Profile::from_env();
    let observability = Observability::install(profile);

    let mut criterion = Criterion::default().configure_from_args();
    build_loop_bench(&mut criterion, profile, &observability);
    criterion.final_summary();
    observability.check_event_writer();
}
