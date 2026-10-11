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
//! To profile an arm, record the bench binary with samply and summarize the hot path:
//!
//! ```sh
//! BUILD_LOOP_PROFILE=production samply record --save-only --unstable-presymbolicate \
//!     -o prof.json.gz -- target/release/deps/build_loop-<hash> --bench <filter> --profile-time 20
//! etc/scripts/perf/summarize_profile.py prof.json.gz --root execute_best_transactions
//! ```

use std::{
    env,
    hint::black_box,
    io, iter,
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
    RestingPredicateMode,
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
const SENDER_BALANCE: u128 = 10u128.pow(24);
const REJECTION_CACHE_TTL: Duration = Duration::from_secs(60);
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

/// Deterministic calldata that compresses poorly, so its estimated DA size tracks its length.
fn incompressible_calldata(seed: usize, len: usize) -> Bytes {
    (0..len.div_ceil(32))
        .flat_map(|chunk| keccak256([seed.to_be_bytes(), chunk.to_be_bytes()].concat()).0)
        .take(len)
        .collect()
}

/// A benchmark transaction before it is signed and wrapped for the pool.
struct PoolTx {
    /// Pool sender id; every benchmark transaction has its own sender at nonce zero.
    sender_id: usize,
    sender: Address,
    to: Address,
    gas_limit: u64,
    priority_fee: u128,
    value: U256,
    input: Bytes,
    predicates: Vec<ValidityPredicate>,
}

impl PoolTx {
    fn into_pooled(self) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit: self.gas_limit,
            max_fee_per_gas: MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: self.priority_fee,
            to: TxKind::Call(self.to),
            value: self.value,
            access_list: Default::default(),
            input: self.input,
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), self.sender),
            encoded_length,
        )
        .with_validity_predicates(self.predicates);

        // Pool validation computes the DA size before a transaction reaches the pool, so the
        // builder reads the cached value. Prime it here to match.
        transaction.estimated_compressed_size();

        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new((self.sender_id as u64).into(), 0),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }
}

/// Priority fee that spreads candidates over a thousand tip levels above `base`.
const fn spread_priority_fee(base: u128, index: usize) -> u128 {
    base + 1_000_000 * (1 + (index as u128 * 7_919) % 1_000)
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

/// Builder context for the next block on top of a parent with production-like gas and base fee.
fn builder_context() -> BasePayloadBuilderCtx {
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
    BasePayloadBuilderCtx::for_test(chain_spec, parent)
}

/// Wraps a pool snapshot the way `payload.rs` does at the start of each flashblock.
fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    best.no_updates();
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        Ordering::coinbase_tip(),
        0,
    )))
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

/// Flashblocks per 2 s mainnet block.
const MAINNET_FLASHBLOCKS: usize = 11;
/// Plain transfers that arrive, and are included, in each flashblock.
const MAINNET_FILLER_PER_FLASHBLOCK: usize = 17;
/// Of each flashblock's filler, how many send value to a watched address and wake its bucket.
const MAINNET_WAKES_PER_FLASHBLOCK: usize = 5;
/// Candidates over the gas limit that stay in the pool and are rejected in every flashblock.
const MAINNET_OVER_GAS_CANDIDATES: usize = 5;
/// Candidates over the per-transaction DA limit that arrive in each flashblock. Their rejection
/// is permanent, so the builder removes them from the pool afterwards.
const MAINNET_OVER_DA_PER_FLASHBLOCK: usize = 5;
/// Parked validity transactions present in every flashblock build.
const MAINNET_PARKED: usize = 2_840;
/// Distinct watched addresses read by the parked transactions' predicates.
const MAINNET_PREDICATE_BUCKETS: usize = 518;
/// `(bucket count, depth)` tiers of the parked transactions: median depth 2, p99 depth 46.
const MAINNET_BUCKET_TIERS: [(usize, usize); 8] =
    [(160, 1), (100, 2), (40, 3), (118, 6), (32, 7), (50, 16), (10, 26), (8, 46)];
/// Coprime with [`MAINNET_PREDICATE_BUCKETS`], so successive wakes hit distinct buckets spread
/// over every depth tier.
const MAINNET_WAKE_STRIDE: usize = 47;
/// Calldata that brings a mainnet transfer to the 459-byte median mainnet transaction size.
const MAINNET_CALLDATA_BYTES: usize = 344;
/// Covers the intrinsic gas of a transfer carrying [`MAINNET_CALLDATA_BYTES`] of calldata.
const MAINNET_TX_GAS_LIMIT: u64 = 30_000;
/// Tip floor for validity transactions, above every other candidate's tip, so each flashblock
/// considers them first.
const MAINNET_VALIDITY_TIP: u128 = 2_000_000_000;

/// Outcomes summed over every flashblock of a block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct BlockTotals {
    considered: u64,
    included: u64,
    deferred: u64,
    rejected_gas: u64,
    rejected_da: u64,
    rejected_other: u64,
}

impl BlockTotals {
    const fn add(&mut self, diag: &FlashblockDiagnostics) {
        self.considered += diag.txs_considered;
        self.included += diag.txs_included;
        self.deferred += diag.txs_deferred;
        self.rejected_gas += diag.txs_rejected_gas;
        self.rejected_da += diag.txs_rejected_da;
        self.rejected_other += diag.txs_rejected_other;
    }
}

/// A whole block shaped like Base mainnet.
///
/// Every flashblock first considers the parked validity transactions, whose balance predicates
/// never match, then includes its filler transfers and rejects its over-limit candidates. A few
/// filler transfers send value to watched addresses, waking their predicate buckets so the
/// parked transactions in them are evaluated again and stay unsatisfied.
struct MainnetBlock {
    ctx: BasePayloadBuilderCtx,
    pre_state: InMemoryDB,
    /// Pending pool at the start of each flashblock: the parked validity transactions and the
    /// over-gas candidates, plus that flashblock's arrivals. Earlier filler and over-DA
    /// candidates are absent because the builder prunes committed and removes permanently
    /// rejected transactions after each flashblock.
    pools: Vec<Pool>,
    /// Number of parked transactions watching each bucket's address.
    bucket_depths: Vec<usize>,
}

impl MainnetBlock {
    const FILLER_START: usize = MAINNET_PARKED;
    const OVER_GAS_START: usize =
        Self::FILLER_START + MAINNET_FLASHBLOCKS * MAINNET_FILLER_PER_FLASHBLOCK;
    const OVER_DA_START: usize = Self::OVER_GAS_START + MAINNET_OVER_GAS_CANDIDATES;
    const SENDER_COUNT: usize =
        Self::OVER_DA_START + MAINNET_FLASHBLOCKS * MAINNET_OVER_DA_PER_FLASHBLOCK;

    fn new() -> Self {
        let mut ctx = builder_context();
        // Evaluating every parked transaction is the work under test. A time budget would also
        // make the outcome counts depend on machine speed.
        ctx.builder_config.predicate_eval_hard_cutoff = Duration::MAX;

        let bucket_depths: Vec<usize> = MAINNET_BUCKET_TIERS
            .iter()
            .flat_map(|&(count, depth)| iter::repeat_n(depth, count))
            .collect();
        assert_eq!(
            bucket_depths.len(),
            MAINNET_PREDICATE_BUCKETS,
            "bucket tiers cover every bucket"
        );
        assert_eq!(
            bucket_depths.iter().sum::<usize>(),
            MAINNET_PARKED,
            "bucket tiers hold every parked transaction"
        );

        let parked: Vec<_> = bucket_depths
            .iter()
            .enumerate()
            .flat_map(|(bucket, &depth)| iter::repeat_n(bucket, depth))
            .enumerate()
            .map(|(sender_id, bucket)| Self::parked_tx(sender_id, bucket))
            .collect();
        let over_gas: Vec<_> = (Self::OVER_GAS_START..Self::OVER_DA_START)
            .map(|sender_id| {
                PoolTx { gas_limit: PARENT_GAS_LIMIT, ..Self::transfer(sender_id) }.into_pooled()
            })
            .collect();

        let pools = (0..MAINNET_FLASHBLOCKS)
            .map(|flashblock| {
                let filler = (0..MAINNET_FILLER_PER_FLASHBLOCK)
                    .map(|slot| Self::filler_tx(flashblock * MAINNET_FILLER_PER_FLASHBLOCK + slot));
                let over_da = (0..MAINNET_OVER_DA_PER_FLASHBLOCK).map(|slot| {
                    Self::over_da_tx(
                        Self::OVER_DA_START + flashblock * MAINNET_OVER_DA_PER_FLASHBLOCK + slot,
                    )
                });
                let mut pool = PendingPool::new(Ordering::coinbase_tip());
                for tx in parked.iter().chain(&over_gas).cloned().chain(filler).chain(over_da) {
                    pool.add_transaction(tx, 0);
                }
                pool
            })
            .collect();

        let mut pre_state = InMemoryDB::default();
        for sender_id in 0..Self::SENDER_COUNT {
            pre_state.insert_account_info(
                Self::sender(sender_id),
                AccountInfo::from_balance(U256::from(SENDER_BALANCE)),
            );
        }

        Self { ctx, pre_state, pools, bucket_depths }
    }

    fn sender(sender_id: usize) -> Address {
        address(5, sender_id)
    }

    fn watched(bucket: usize) -> Address {
        address(6, bucket)
    }

    /// Bucket whose watched address the `wake`-th wake of the block sends value to.
    const fn woken_bucket(wake: usize) -> usize {
        wake * MAINNET_WAKE_STRIDE % MAINNET_PREDICATE_BUCKETS
    }

    /// A median-sized transfer to a fresh account, tipped below every validity transaction.
    fn transfer(sender_id: usize) -> PoolTx {
        PoolTx {
            sender_id,
            sender: Self::sender(sender_id),
            to: address(7, sender_id),
            gas_limit: MAINNET_TX_GAS_LIMIT,
            priority_fee: spread_priority_fee(0, sender_id),
            value: U256::from(1 + sender_id),
            input: incompressible_calldata(sender_id, MAINNET_CALLDATA_BYTES),
            predicates: Vec::new(),
        }
    }

    /// A validity transaction waiting for its bucket's watched balance to reach an unreachable
    /// value.
    fn parked_tx(
        sender_id: usize,
        bucket: usize,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        PoolTx {
            priority_fee: spread_priority_fee(MAINNET_VALIDITY_TIP, sender_id),
            predicates: vec![ValidityPredicate::Balance {
                address: Self::watched(bucket),
                op: ValidityOperator::Equal,
                value: U256::MAX,
            }],
            ..Self::transfer(sender_id)
        }
        .into_pooled()
    }

    /// The `filler`-th filler transfer of the block. The first few of each flashblock wake a
    /// bucket.
    fn filler_tx(filler: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let sender_id = Self::FILLER_START + filler;
        let flashblock = filler / MAINNET_FILLER_PER_FLASHBLOCK;
        let slot = filler % MAINNET_FILLER_PER_FLASHBLOCK;
        let mut tx = Self::transfer(sender_id);
        if slot < MAINNET_WAKES_PER_FLASHBLOCK {
            let wake = flashblock * MAINNET_WAKES_PER_FLASHBLOCK + slot;
            tx.to = Self::watched(Self::woken_bucket(wake));
        }
        tx.into_pooled()
    }

    fn over_da_tx(sender_id: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        PoolTx {
            gas_limit: 200_000,
            input: incompressible_calldata(sender_id, OVERSIZED_CALLDATA_BYTES),
            ..Self::transfer(sender_id)
        }
        .into_pooled()
    }

    /// Cumulative limits that admit exactly the filler that has arrived by `flashblock`.
    fn limits(flashblock: usize) -> ResourceLimits {
        ResourceLimits {
            block_gas_limit: ((flashblock + 1) * MAINNET_FILLER_PER_FLASHBLOCK) as u64
                * MAINNET_TX_GAS_LIMIT,
            tx_data_limit: Some(TX_DATA_LIMIT),
            ..Default::default()
        }
    }

    fn fresh_state(&self) -> State<InMemoryDB> {
        State::builder().with_database(self.pre_state.clone()).with_bundle_update().build()
    }

    /// An empty rejection cache. Created outside the timed region, as the builder's cache
    /// outlives every block.
    fn rejection_cache(&self) -> RejectionCache {
        RejectionCache::new(Self::SENDER_COUNT as u64, REJECTION_CACHE_TTL)
    }

    /// Builds every flashblock of the block, driving the candidate iterator as `payload.rs`
    /// does between flashblocks.
    ///
    /// `rejection_cache` stands in for the builder's long-lived cache, which each payload job
    /// clones.
    fn build(
        &self,
        state: &mut State<InMemoryDB>,
        rejection_cache: &RejectionCache,
        mode: RestingPredicateMode,
    ) -> BlockTotals {
        let mut info = ExecutionInfo::default();
        let mut deferrals = BlockDeferrals::default();
        let mut totals = BlockTotals::default();
        let mut best = BestFlashblocksTxs::new(parkable(&self.pools[0]), rejection_cache.clone())
            .with_resting_predicate_mode(mode);

        for (flashblock, pool) in self.pools.iter().enumerate() {
            if flashblock > 0 {
                best.refresh_iterator(parkable(pool));
            }
            let committed_until = info.executed_transactions.len();
            let diag = self
                .ctx
                .execute_best_transactions(
                    &mut info,
                    &mut deferrals,
                    state,
                    &mut best,
                    &Self::limits(flashblock),
                )
                .expect("build loop succeeds");
            let committed: Vec<_> = info.executed_transactions[committed_until..]
                .iter()
                .map(|tx| tx.tx_hash())
                .collect();
            best.mark_committed(&committed);
            if !diag.permanently_rejected_txs.is_empty() {
                best.mark_rejected(&diag.permanently_rejected_txs);
            }
            totals.add(&diag);
        }
        totals
    }

    /// Outcomes the workload is designed to produce.
    ///
    /// With resting predicates off, every flashblock defers every parked transaction. Enforced,
    /// only the first flashblock does; later ones defer just the transactions a wake promotes.
    /// Buckets at or above the ordered threshold wake only transactions whose `Equal` value is
    /// reached, so a wake promotes none of theirs. The resting index uses the same default
    /// threshold as the builder config.
    fn expected_totals(&self, mode: RestingPredicateMode) -> BlockTotals {
        let ordered_threshold = self.ctx.builder_config.predicate_bucket_ordered_threshold;
        let deferred = match mode {
            RestingPredicateMode::Off => MAINNET_PARKED * MAINNET_FLASHBLOCKS,
            RestingPredicateMode::Enforce => {
                let rewoken: usize = (MAINNET_WAKES_PER_FLASHBLOCK
                    ..MAINNET_FLASHBLOCKS * MAINNET_WAKES_PER_FLASHBLOCK)
                    .map(|wake| self.bucket_depths[Self::woken_bucket(wake)])
                    .filter(|&depth| depth < ordered_threshold)
                    .sum();
                MAINNET_PARKED + rewoken
            }
            RestingPredicateMode::Shadow => {
                unimplemented!("the mainnet block measures Off and Enforce")
            }
        } as u64;
        let included = (MAINNET_FLASHBLOCKS * MAINNET_FILLER_PER_FLASHBLOCK) as u64;
        let rejected_gas = (MAINNET_FLASHBLOCKS * MAINNET_OVER_GAS_CANDIDATES) as u64;
        let rejected_da = (MAINNET_FLASHBLOCKS * MAINNET_OVER_DA_PER_FLASHBLOCK) as u64;
        BlockTotals {
            considered: deferred + included + rejected_gas + rejected_da,
            included,
            deferred,
            rejected_gas,
            rejected_da,
            rejected_other: 0,
        }
    }
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
