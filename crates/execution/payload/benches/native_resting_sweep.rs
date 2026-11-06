//! Per-block cost of the native (post-Denim) payload builder selecting from the production Base
//! transaction pool while it holds a resting validity population.
//!
//! The pool is the production `BaseTransactionPool` (see `fixtures/production_pool.rs`). It
//! holds a population of high-priority validity transactions whose predicates stay unsatisfied,
//! plus [`FILLER_PER_BLOCK`] fresh ordinary transfers admitted before every block. Each block
//! runs the native builder's calls in order: begin a build against the pool's dormancy index on
//! the block's parent with the pre-selection writes, or, when the pool keeps no index and the
//! builder's resting mode is on, carry the builder's resting set into the block; build the pool
//! iterator; run the selection loop, which parks unsatisfied validity transactions in the pool's
//! index and wakes them by each commit's writes; then seal the dormancy build or store the resting
//! set under the block's hash. Between blocks the pool mines the previous block through the node's
//! canonical update, which also moves the base fee, and runs the node's canonical state-diff
//! maintenance for that block, which checks the dormancy tip and classifies newly admitted
//! validity transactions against the provider's state.
//! The matrix of populations, lane kinds, liveness cases and modes, the phases each block is split
//! into, and the environment that selects them are described in `fixtures/resting_matrix.rs`.
//!
//! Every block asserts that it includes all of its ordinary traffic and nothing else, plus the
//! woken lanes in the liveness case's wake block and the churn case's satisfied lanes, each of
//! which the block must include. When a mode withholds resting transactions, the
//! last block's probe must yield nothing but executable transactions, so the population is
//! withheld from the iterator's input rather than yielded and skipped; under pool dormancy the
//! pool's dormant gauge must also count every lane still resting. In the discard case these hold
//! for the last block built on the last sealed block. The liveness case also asserts
//! that the pool expired its expiring lanes.
//!
//! This measures exhaustive selection work, not throughput under a latency deadline. Both modes
//! use an unlimited predicate-evaluation budget: the production 10 ms cutoff can defer executable
//! lanes after classification work and would make the inclusion assertions depend on host speed.
//! Every block also asserts that predicate evaluation was never cut short.
//!
//! The `demo` case, which runs only when selected, admits a parked population of up to millions
//! of validity transactions and classifies it before the first build, then ingests parked,
//! valid-at-admission, and same-block-satisfied validity transactions every block; see
//! [`run_demo`] and [`Demo`]. Its blocks assert that they include every valid and satisfied
//! transaction and no parked one, and it reports `SETUP` throughput and resident memory, a `ROW`
//! per block, and a `DEMO` summary of build percentiles against the 50 ms budget.
//!
//! With `RESTING_MATRIX_DEMO_SUBMITTERS` set the demo runs concurrently instead; see
//! [`run_concurrent_demo`]. It reports `SETUP` as before, a `CROW` per block, and a `CONCURRENT`
//! summary of achieved ingest, admission latency, build and maintenance percentiles.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! RESTING_MATRIX_CASES=protocol RESTING_MATRIX_POPULATIONS=75000 \
//!   RESTING_MATRIX_MODES=enforce,off,builder_enforce \
//!   cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! RESTING_MATRIX_CASES=demo RESTING_MATRIX_POPULATIONS=1000000 RESTING_MATRIX_BLOCKS=10 \
//!   cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! RESTING_MATRIX_CASES=demo RESTING_MATRIX_POPULATIONS=1000000 RESTING_MATRIX_BLOCKS=15 \
//!   RESTING_MATRIX_DEMO_SUBMITTERS=4 RESTING_MATRIX_DEMO_RATE=50000 \
//!   cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! ```

#[path = "fixtures/production_pool.rs"]
pub mod production_pool;
#[path = "fixtures/resting_matrix.rs"]
pub mod resting_matrix;

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_evm::Evm as _;
use alloy_primitives::{Address, B256, TxHash, U256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, ParkableBestPayloadTransactions,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{ParkableTransactionPool, ValidityDormancyMode};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use production_pool::{Accounts, ProductionPool, TRANSFER_GAS, watched_balance_is_one};
use resting_matrix::{
    BlockTiming, Case, ConcurrentBlock, ConcurrentIngest, DEMO_BLOCK_INTERVAL, Demo, DemoBlock,
    DemoConcurrency, DemoSetup, Matrix, Mode, Phase, Probe, Report, block_hash, fund_woken,
    parent_base_fee, resident_bytes, sibling_hash, timed_drop, wake_transitions,
};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::SealedHeader;
use reth_revm::db::State;
use revm::{
    database::{BundleState, InMemoryDB, TransitionState, states::bundle_state::BundleRetention},
    state::AccountInfo,
};

type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

const CHAIN_ID: u64 = 8_453;
/// Ordinary transfers admitted before, and included in, each block.
const FILLER_PER_BLOCK: usize = 120;
const DENIM_TIMESTAMP: u64 = 1;
/// The pool's dormant-transaction gauge as Prometheus renders it.
const DORMANT_GAUGE: &str = "txpool_validity_dormant_transactions";
/// Predicates the native build loop evaluated and found unsatisfied, as Prometheus renders it.
const UNSATISFIED_EVALUATIONS: &str =
    "base_builder_validity_predicate_evaluations_total{outcome=\"not_satisfied\"}";
/// Builds whose predicate evaluation budget ran out, as Prometheus renders it.
const CUTOFF_BUILDS: &str = "base_builder_validity_predicate_eval_cutoff_builds_total";
/// Bit position above which a concurrent demo submitter's number sits in its validity sender
/// indices, keeping each submitter's senders apart from the setup population, the build thread's
/// senders, and every other submitter's.
const SUBMITTER_SENDER_SHIFT: u32 = 40;

/// The native builder's resting-predicate store calls at this revision.
mod selection {
    use std::time::Duration;

    use alloy_primitives::B256;
    use base_execution_payload_builder::{
        DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, RestingPredicateStore, SharedRestingPredicates,
    };
    use revm::database::TransitionState;

    /// The native builder's `RESTING_SWEEP_WAIT_LIMIT`.
    const SWEEP_WAIT_LIMIT: Duration = Duration::from_millis(25);

    pub(super) fn store() -> RestingPredicateStore {
        RestingPredicateStore::new(reth_tasks::Runtime::test())
    }

    pub(super) fn begin(
        store: &RestingPredicateStore,
        parent: B256,
        block: u64,
        pre_block_changes: &TransitionState,
    ) -> SharedRestingPredicates {
        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
        resting.begin_block(parent, block, Some(pre_block_changes));
        SharedRestingPredicates::new(resting)
    }

    pub(super) fn put(
        store: &RestingPredicateStore,
        resting: SharedRestingPredicates,
        block_hash: B256,
    ) {
        store.put(store.finish(resting.take_predicates()), block_hash);
    }
}

fn chain_spec() -> Arc<BaseChainSpec> {
    Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    )
}

/// A Denim payload context for `block` with `gas_limit`, whose parent carries the block's scheduled
/// base fee.
fn payload_context(chain_spec: &Arc<BaseChainSpec>, mode: Mode, block: u64, gas_limit: u64) -> Ctx {
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        number: block - 1,
        gas_limit,
        base_fee_per_gas: Some(parent_base_fee(block)),
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
        gas_limit: Some(gas_limit),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::base(Arc::clone(chain_spec)),
        builder_config: BaseBuilderConfig {
            predicate_eval_hard_cutoff: Duration::MAX,
            ..BaseBuilderConfig::default().with_resting_predicate_mode(mode.resting())
        },
        chain_spec: Arc::clone(chain_spec),
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

fn fund(db: &mut InMemoryDB, address: Address) {
    db.insert_account_info(
        address,
        AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
    );
}

/// The value of the unlabelled or fully labelled series `series`, zero before it is first set.
fn metric(metrics: &PrometheusHandle, series: &str) -> usize {
    metrics
        .render()
        .lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.trim().parse::<f64>().ok())
        .map_or(0, |value| value as usize)
}

/// The pool's dormant-transaction gauge, zero before the pool first sets it.
fn dormant_gauge(metrics: &PrometheusHandle) -> usize {
    metric(metrics, DORMANT_GAUGE)
}

/// Panics if any build so far cut predicate evaluation short.
fn assert_exhaustive_selection(metrics: &PrometheusHandle) {
    assert_eq!(
        metric(metrics, CUTOFF_BUILDS),
        0,
        "exhaustive selection must not exhaust the predicate evaluation budget"
    );
}

/// Runs one case of the matrix and returns its block timings.
fn run_case(
    report: Report,
    metrics: &PrometheusHandle,
    matrix: &Matrix,
    chain_spec: &Arc<BaseChainSpec>,
    case: Case,
    mode: Mode,
    population: usize,
) -> Vec<BlockTiming> {
    let pool = ProductionPool::new(CHAIN_ID, mode.dormancy());
    let _runtime = pool.enter();
    let lanes = case.lanes(population);
    let churn = case.churn(matrix.churn, lanes);
    let start = Instant::now();
    let (mut resting_hashes, mut resting_senders) =
        pool.admit_resting(case.kind(), 0..lanes, |lane| case.predicates(lane));
    report.setup(case, mode, population, lanes, start.elapsed());

    let mut db = InMemoryDB::default();
    for sender in &resting_senders {
        fund(&mut db, *sender);
    }
    let gas_limit = (FILLER_PER_BLOCK + case.headroom(churn)) as u64 * TRANSFER_GAS;
    let mut live: VecDeque<usize> = (0..lanes).collect();
    let mut satisfiers = 0;
    let store = selection::store();
    let mut mined: (Vec<TxHash>, Vec<Address>, BundleState) = Default::default();
    let mut held = None;
    let mut previous_base_fee = 0;
    let mut timings = Vec::new();

    for block in 1..=matrix.blocks {
        let first_ordinary = (block as usize - 1) * FILLER_PER_BLOCK;
        for index in first_ordinary..first_ordinary + FILLER_PER_BLOCK {
            fund(&mut db, Accounts::Ordinary.address(index));
        }
        let woken = case.woken(block);
        fund_woken(&mut db, woken.clone());
        let (replaced, satisfied) =
            if block > 1 { churn.take(&mut live) } else { Default::default() };
        let fresh = resting_hashes.len()..resting_hashes.len() + replaced.len() + satisfied.len();
        let fresh_senders: Vec<Address> =
            fresh.clone().map(|lane| Accounts::Validity.address(lane)).collect();
        let satisfier_senders: Vec<Address> = (satisfiers..satisfiers + satisfied.len())
            .map(|index| Accounts::Satisfier.address(index))
            .collect();
        for sender in fresh_senders.iter().chain(&satisfier_senders) {
            fund(&mut db, *sender);
        }

        let ctx = payload_context(chain_spec, mode, block, gas_limit);
        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let attributes = ctx.best_transaction_attributes(builder.evm_mut().block());

        let mut mutation = Default::default();
        let mut dormancy = Default::default();
        if block > 1 {
            let (hashes, senders, bundle) = std::mem::take(&mut mined);
            let start = Instant::now();
            pool.mine(block - 1, previous_base_fee, attributes.basefee, hashes, &senders);
            if !replaced.is_empty() {
                pool.remove(replaced.iter().map(|&lane| resting_hashes[lane]).collect());
            }
            let (hashes, senders) =
                pool.admit_resting(case.kind(), fresh.clone(), |lane| case.predicates(lane));
            mutation = start.elapsed();
            assert_eq!(senders, fresh_senders, "the build state funds every new resting sender");
            resting_hashes.extend(hashes);
            resting_senders.extend(senders);
            live.extend(fresh.clone());
            let start = Instant::now();
            pool.maintain_canonical(BlockNumHash::new(block - 1, block_hash(block - 1)), &bundle);
            dormancy = start.elapsed();
        }
        let start = Instant::now();
        let (mut ordinary, mut ordinary_senders) =
            pool.admit_ordinary(first_ordinary, FILLER_PER_BLOCK);
        let (satisfier_hashes, admitted_satisfiers) = pool.admit_satisfiers(satisfiers, &satisfied);
        mutation += start.elapsed();
        assert_eq!(admitted_satisfiers, satisfier_senders, "the build state funds every satisfier");
        satisfiers += satisfied.len();
        ordinary.extend(satisfier_hashes);
        ordinary_senders.extend(satisfier_senders);
        let held_drop = held.take().map(timed_drop).unwrap_or_default();
        if case.expired_by() == Some(block) {
            for lane in case.expiring() {
                assert!(
                    !pool.contains(&resting_hashes[lane]),
                    "the pool must expire lane {lane} after its deadline"
                );
            }
        }

        let discarded = case.discards(block, matrix.discard_interval);
        let parent = if discarded { sibling_hash(block - 1) } else { block_hash(block - 1) };
        let pre_block_changes = wake_transitions(woken.clone());
        let start = Instant::now();
        let dormancy_build =
            pool.pool.begin_dormancy_build(parent, block, Some(&pre_block_changes));
        let mut resting = (dormancy_build.is_none() && mode.resting().is_enabled())
            .then(|| selection::begin(&store, parent, block, &pre_block_changes));
        let carry = start.elapsed();
        let members = resting.as_ref().map_or(0, |resting| resting.len());
        // The gauge is process-wide; only a dormancy-enforcing pool set it during this case.
        let dormant = if mode.dormancy() == ValidityDormancyMode::Enforce {
            dormant_gauge(metrics)
        } else {
            0
        };

        let hold = || {
            resting
                .as_ref()
                .filter(|_| mode.resting().is_enforced())
                .map(|resting| resting.iterator_hold())
        };
        let start = Instant::now();
        let best = pool.pool.best_transactions_with_attributes_and_parking(
            attributes,
            hold(),
            dormancy_build.as_ref(),
        );
        let construct = start.elapsed();
        let executable = FILLER_PER_BLOCK + woken.len() + 2 * satisfied.len();
        let probe = Probe::run(|| {
            pool.pool.best_transactions_with_attributes_and_parking(
                attributes,
                hold(),
                dormancy_build.as_ref(),
            )
        });
        // Lanes admitted since the previous build reach a builder-enforcing iterator once, while
        // the pool classifies them before the build.
        let unclassified = if dormancy_build.is_some() { 0 } else { fresh.len() };
        let phase =
            Phase::of(mode, probe.yielded, executable + unclassified, !woken.is_empty(), discarded);
        let probe_drop = if case.holds_iterator() {
            held = Some(probe.iterator);
            Default::default()
        } else {
            timed_drop(probe.iterator)
        };

        let mut info = ExecutionInfo::default();
        let start = Instant::now();
        ctx.execute_best_transactions(
            &mut info,
            &mut builder,
            ParkableBestPayloadTransactions::new(best),
            resting.as_ref(),
        )
        .expect("in-memory execution cannot fail");
        let selection_time = start.elapsed();
        let included = builder.executor().receipts().len();
        assert_exhaustive_selection(metrics);
        assert_eq!(included, executable, "block {block} must include every executable transaction");

        let start = Instant::now();
        if let Some(resting) = resting.take() {
            selection::put(&store, resting, block_hash(block));
        }
        if let Some(dormancy_build) = dormancy_build {
            dormancy_build.seal(block_hash(block));
        }
        let store_time = start.elapsed();

        drop(builder);
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        for &lane in &satisfied {
            assert_eq!(
                bundle
                    .account(&resting_senders[lane])
                    .and_then(|account| account.info.as_ref())
                    .map(|info| info.nonce),
                Some(1),
                "block {block} must include satisfied lane {lane}"
            );
        }

        mined = (ordinary, ordinary_senders, bundle);
        for lane in woken.chain(satisfied) {
            mined.0.push(resting_hashes[lane]);
            mined.1.push(resting_senders[lane]);
        }
        previous_base_fee = attributes.basefee;
        let timing = BlockTiming {
            block,
            phase,
            members,
            dormant,
            probe_yielded: probe.yielded,
            included,
            mutation,
            dormancy,
            held_drop,
            carry,
            probe_construct: probe.construct,
            probe_first_yield: probe.first_yield,
            probe_drain: probe.drain,
            probe_drop,
            construct,
            selection: selection_time,
            store: store_time,
            ..Default::default()
        };
        report.row(case, mode, population, &timing);
        timings.push(timing);
        if let Some(gap) = matrix.gap {
            std::thread::sleep(gap);
        }
    }

    let last = timings
        .iter()
        .rev()
        .find(|timing| timing.phase != Phase::Discard)
        .expect("the matrix builds blocks on the last sealed block");
    if mode.withholds() && lanes > 0 {
        assert_eq!(
            last.phase,
            Phase::Steady,
            "the resting population must be withheld from block {}'s iterator",
            last.block
        );
    }
    if mode.dormancy() == ValidityDormancyMode::Enforce && lanes > 0 {
        assert_eq!(
            last.dormant,
            // A block's satisfied lanes are still dormant when its build starts, beside the new
            // lanes that replace them.
            case.final_members(lanes) + churn.satisfied,
            "dormant validity transactions at block {}",
            last.block
        );
    }
    timings
}

/// Runs the demo case (see [`Demo`]) and reports its setup, every block, and its summary.
///
/// Before the first block the pool admits the parked population, streaming it through validation,
/// and classifies it in canonical maintenance on a sealed tip, as a node's builder leaves one.
/// Each block then admits [`Demo::early_arrivals`] and the block's same-block-satisfiable
/// transactions, runs the canonical maintenance for the previous block, admits
/// [`Demo::late_arrivals`], the valid-at-admission transactions, the ordinary traffic and the
/// satisfying transfers, and builds. Nothing generated is kept past admission except the hashes
/// the next canonical update mines.
fn run_demo(
    report: Report,
    metrics: &PrometheusHandle,
    matrix: &Matrix,
    chain_spec: &Arc<BaseChainSpec>,
    mode: Mode,
    population: usize,
) {
    let demo = matrix.demo;
    let enforced = mode.dormancy() == ValidityDormancyMode::Enforce;
    let pool = ProductionPool::new(CHAIN_ID, mode.dormancy());
    let _runtime = pool.enter();
    let genesis = BlockNumHash::new(0, block_hash(0));
    if let Some(build) = pool.pool.begin_dormancy_build(B256::ZERO, genesis.number, None) {
        build.seal(genesis.hash);
    }

    let mut setup =
        DemoSetup { parked: population, rss_base: resident_bytes(), ..Default::default() };
    let start = Instant::now();
    pool.fund_validity(0..population);
    setup.funding = start.elapsed();
    setup.rss_funded = resident_bytes();
    let start = Instant::now();
    pool.admit_validity(0..population, |sender| demo.parked_predicates(sender), |_| ());
    setup.admission = start.elapsed();
    setup.rss_admitted = resident_bytes();
    let start = Instant::now();
    pool.maintain_canonical(genesis, &BundleState::default());
    setup.classification = enforced.then(|| start.elapsed());
    setup.rss_classified = resident_bytes();
    report.demo_setup(mode, population, &setup);
    if enforced {
        assert_eq!(
            dormant_gauge(metrics),
            population,
            "maintenance before the first build classifies every parked transaction dormant"
        );
    }
    if matrix.demo_concurrency.is_some() {
        run_concurrent_demo(report, metrics, matrix, chain_spec, mode, population, &pool);
        return;
    }

    let executable = FILLER_PER_BLOCK + demo.valid + 2 * demo.satisfied;
    let gas_limit = executable as u64 * TRANSFER_GAS;
    let mut db = InMemoryDB::default();
    let mut next_sender = population;
    let mut satisfied_so_far = 0;
    let store = selection::store();
    let mut mined: (Vec<TxHash>, Vec<Address>, BundleState) = Default::default();
    let mut previous_base_fee = 0;
    let mut blocks = Vec::new();

    for block in 1..=matrix.blocks {
        let early = next_sender..next_sender + demo.early_arrivals();
        let satisfied = early.end..early.end + demo.satisfied;
        let late = satisfied.end..satisfied.end + demo.late_arrivals();
        let valid = late.end..late.end + demo.valid;
        next_sender = valid.end;
        let first_ordinary = (block as usize - 1) * FILLER_PER_BLOCK;
        let satisfiers = satisfied_so_far..satisfied_so_far + demo.satisfied;
        let satisfied_watched: Vec<usize> =
            satisfiers.clone().map(|target| demo.target_watched(target)).collect();
        for sender in satisfied.clone().chain(valid.clone()) {
            fund(&mut db, Accounts::Validity.address(sender));
        }
        for index in first_ordinary..first_ordinary + FILLER_PER_BLOCK {
            fund(&mut db, Accounts::Ordinary.address(index));
        }
        for index in satisfiers.clone() {
            fund(&mut db, Accounts::Satisfier.address(index));
        }

        let ctx = payload_context(chain_spec, mode, block, gas_limit);
        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let attributes = ctx.best_transaction_attributes(builder.evm_mut().block());

        let (hashes, senders, bundle) = std::mem::take(&mut mined);
        let start = Instant::now();
        if block > 1 {
            pool.mine(block - 1, previous_base_fee, attributes.basefee, hashes, &senders);
        }
        let canonical_update = start.elapsed();
        let start = Instant::now();
        pool.fund_validity(early.start..satisfied.end);
        pool.admit_validity(early, |sender| demo.parked_predicates(sender), |_| ());
        let mut satisfied_hashes = Vec::with_capacity(demo.satisfied);
        pool.admit_validity(
            satisfied.clone(),
            |sender| vec![watched_balance_is_one(satisfied_watched[sender - satisfied.start])],
            |hash| satisfied_hashes.push(hash),
        );
        let mut mutation = start.elapsed();
        let start = Instant::now();
        pool.maintain_canonical(BlockNumHash::new(block - 1, block_hash(block - 1)), &bundle);
        let dormancy = start.elapsed();

        let start = Instant::now();
        pool.fund_validity(late.start..valid.end);
        pool.admit_validity(late, |sender| demo.parked_predicates(sender), |_| ());
        let mut valid_hashes = Vec::with_capacity(demo.valid);
        pool.admit_validity(valid.clone(), Demo::valid_predicates, |hash| valid_hashes.push(hash));
        let (ordinary, ordinary_senders) = pool.admit_ordinary(first_ordinary, FILLER_PER_BLOCK);
        let (satisfier_hashes, satisfier_senders) =
            pool.admit_satisfiers(satisfiers.start, &satisfied_watched);
        mutation += start.elapsed();

        let parent = block_hash(block - 1);
        let pre_block_changes = TransitionState::default();
        let start = Instant::now();
        let dormancy_build =
            pool.pool.begin_dormancy_build(parent, block, Some(&pre_block_changes));
        let mut resting = (dormancy_build.is_none() && mode.resting().is_enabled())
            .then(|| selection::begin(&store, parent, block, &pre_block_changes));
        let carry = start.elapsed();
        let members = resting.as_ref().map_or(0, |resting| resting.len());
        let dormant = if enforced { dormant_gauge(metrics) } else { 0 };

        let hold = || {
            resting
                .as_ref()
                .filter(|_| mode.resting().is_enforced())
                .map(|resting| resting.iterator_hold())
        };
        let start = Instant::now();
        let best = pool.pool.best_transactions_with_attributes_and_parking(
            attributes,
            hold(),
            dormancy_build.as_ref(),
        );
        let construct = start.elapsed();
        let probe = Probe::run(|| {
            pool.pool.best_transactions_with_attributes_and_parking(
                attributes,
                hold(),
                dormancy_build.as_ref(),
            )
        });
        let unclassified =
            if dormancy_build.is_some() { demo.late_arrivals() } else { demo.arrivals };
        let phase = Phase::of(mode, probe.yielded, executable + unclassified, false, false);
        let probe_drop = timed_drop(probe.iterator);

        let evaluated_before = metric(metrics, UNSATISFIED_EVALUATIONS);
        let mut info = ExecutionInfo::default();
        let start = Instant::now();
        ctx.execute_best_transactions(
            &mut info,
            &mut builder,
            ParkableBestPayloadTransactions::new(best),
            resting.as_ref(),
        )
        .expect("in-memory execution cannot fail");
        let selection_time = start.elapsed();
        let parked_evaluated = metric(metrics, UNSATISFIED_EVALUATIONS) - evaluated_before;
        let included = builder.executor().receipts().len();
        assert_exhaustive_selection(metrics);

        let start = Instant::now();
        if let Some(resting) = resting.take() {
            selection::put(&store, resting, block_hash(block));
        }
        if let Some(dormancy_build) = dormancy_build {
            dormancy_build.seal(block_hash(block));
        }
        let store_time = start.elapsed();

        drop(builder);
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let committed = |address: Address| {
            bundle
                .account(&address)
                .and_then(|account| account.info.as_ref())
                .is_some_and(|info| info.nonce == 1)
        };
        let valid_included =
            valid.clone().filter(|&sender| committed(Accounts::Validity.address(sender))).count();
        let satisfied_included = satisfied
            .clone()
            .filter(|&sender| committed(Accounts::Validity.address(sender)))
            .count();
        assert_eq!(
            valid_included, demo.valid,
            "block {block} must include every valid transaction"
        );
        assert_eq!(
            satisfied_included, demo.satisfied,
            "block {block} must include every transaction satisfied in it"
        );
        assert!(
            ordinary_senders.iter().chain(&satisfier_senders).all(|&sender| committed(sender)),
            "block {block} must include all of its ordinary traffic"
        );
        assert_eq!(
            included, executable,
            "block {block} includes its executable transactions and no parked one"
        );

        let validity_senders =
            satisfied.chain(valid).map(|sender| Accounts::Validity.address(sender));
        mined = (
            [ordinary, satisfier_hashes, valid_hashes, satisfied_hashes].concat(),
            ordinary_senders.into_iter().chain(satisfier_senders).chain(validity_senders).collect(),
            bundle,
        );
        satisfied_so_far = satisfiers.end;
        previous_base_fee = attributes.basefee;
        let timing = BlockTiming {
            block,
            phase,
            members,
            dormant,
            probe_yielded: probe.yielded,
            included,
            mutation,
            dormancy,
            carry,
            probe_construct: probe.construct,
            probe_first_yield: probe.first_yield,
            probe_drain: probe.drain,
            probe_drop,
            construct,
            selection: selection_time,
            store: store_time,
            ..Default::default()
        };
        let demo_block = DemoBlock {
            arrivals: demo.arrivals,
            parked_evaluated,
            valid_included,
            satisfied_included,
            maintenance: canonical_update + dormancy,
        };
        report.demo_row(mode, population, &timing, &demo_block);
        blocks.push((timing, demo_block));
        if let Some(gap) = matrix.gap {
            std::thread::sleep(gap);
        }
    }
    report.demo_summary(mode, population, &demo, &setup, &blocks, resident_bytes());
}

/// A sealed block the maintenance thread tells the pool about, as the node's pool maintenance does
/// once the block is canonical.
#[derive(Debug)]
pub struct CanonicalBlock {
    /// Block number.
    pub number: u64,
    /// The block's base fee.
    pub base_fee: u64,
    /// The next block's base fee.
    pub pending_base_fee: u64,
    /// Hashes of the transactions the block included.
    pub mined: Vec<TxHash>,
    /// Senders of the transactions the block included.
    pub senders: Vec<Address>,
    /// The block's state changes.
    pub bundle: BundleState,
}

/// When one block's maintenance ran on the maintenance thread.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceRun {
    /// The maintained block.
    pub block: u64,
    /// When the maintenance began.
    pub started: Instant,
    /// When the maintenance ended.
    pub finished: Instant,
}

/// Runs the canonical update and the state-diff maintenance for every block `canonical` delivers,
/// in order, until the build thread hangs up, as the node's maintenance task does off the build
/// path.
fn maintain(
    pool: &ProductionPool,
    canonical: mpsc::Receiver<CanonicalBlock>,
) -> Vec<MaintenanceRun> {
    let _runtime = pool.enter();
    canonical
        .into_iter()
        .map(|block| {
            let started = Instant::now();
            pool.mine(
                block.number,
                block.base_fee,
                block.pending_base_fee,
                block.mined,
                &block.senders,
            );
            pool.maintain_canonical(
                BlockNumHash::new(block.number, block_hash(block.number)),
                &block.bundle,
            );
            MaintenanceRun { block: block.number, started, finished: Instant::now() }
        })
        .collect()
}

/// One concurrent demo submitter: admits parked validity transactions from senders of its own,
/// [`DemoConcurrency::batch`] per call of the pool's async admission API, paced to its share of
/// [`DemoConcurrency::rate`] when set, until `stop`. Adds each batch to `admitted` once the pool
/// accepted it and returns every call's latency, which excludes generating the batch and funding
/// its senders.
fn submit_parked(
    pool: &ProductionPool,
    demo: Demo,
    concurrency: DemoConcurrency,
    submitter: usize,
    stop: &AtomicBool,
    admitted: &AtomicUsize,
) -> Vec<Duration> {
    let interval = concurrency.batch_interval();
    let mut next_sender = (submitter + 1) << SUBMITTER_SENDER_SHIFT;
    let mut due = Instant::now();
    let mut latencies = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if let Some(interval) = interval {
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            // A submitter behind its schedule admits at once but does not burst to catch up.
            due = (due + interval).max(Instant::now());
        }
        let senders = next_sender..next_sender + concurrency.batch;
        next_sender = senders.end;
        pool.fund_validity(senders.clone());
        let transactions: Vec<_> = senders
            .map(|sender| pool.validity_transaction(sender, demo.parked_predicates(sender)))
            .collect();
        let start = Instant::now();
        pool.admit(transactions);
        latencies.push(start.elapsed());
        admitted.fetch_add(concurrency.batch, Ordering::Relaxed);
    }
    latencies
}

/// The base fee the build of `block` runs at, which the canonical update of the block before it
/// moves the pool to.
fn scheduled_base_fee(
    chain_spec: &Arc<BaseChainSpec>,
    mode: Mode,
    block: u64,
    gas_limit: u64,
) -> u64 {
    let ctx = payload_context(chain_spec, mode, block, gas_limit);
    let mut state = State::builder().with_database(InMemoryDB::default()).build();
    state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
    let mut builder = ctx.block_builder(&mut state).expect("block builder");
    ctx.best_transaction_attributes(builder.evm_mut().block()).basefee
}

/// The concurrent demo's build thread: what it builds from and where it hands sealed blocks.
#[derive(Debug)]
pub struct ConcurrentBuilder<'a> {
    /// The pool the submitters admit into.
    pub pool: &'a ProductionPool,
    /// The process's metrics.
    pub metrics: &'a PrometheusHandle,
    /// The chain the blocks build on.
    pub chain_spec: &'a Arc<BaseChainSpec>,
    /// The run's mode.
    pub mode: Mode,
    /// Per-block ingest and predicate layout.
    pub demo: Demo,
    /// The parked population admitted at setup, below the build thread's senders.
    pub population: usize,
    /// Blocks to build.
    pub blocks: u64,
    /// Parked transactions the submitters admitted so far.
    pub admitted: &'a AtomicUsize,
    /// Set once the last block ends, stopping the submitters.
    pub stop: &'a AtomicBool,
    /// The maintenance thread's queue of sealed blocks.
    pub canonical: mpsc::Sender<CanonicalBlock>,
}

impl ConcurrentBuilder<'_> {
    /// Builds every block on a [`DEMO_BLOCK_INTERVAL`] cadence, starting a block at once when the
    /// previous one overran its slot, then stops the submitters. Each block admits its valid,
    /// satisfied, ordinary and satisfying transactions on this thread just before it builds
    /// through the production native path (begin on the tip, snapshot, select with a wake per
    /// commit, seal), and hands the sealed block to the maintenance thread without waiting for it.
    /// Returns every block with when its build began, the parked transactions admitted from the
    /// first slot until the stop, and how long that took.
    pub fn run(self) -> (Vec<(ConcurrentBlock, Instant)>, usize, Duration) {
        let pool = self.pool;
        let demo = self.demo;
        let enforced = self.mode.dormancy() == ValidityDormancyMode::Enforce;
        let executable = FILLER_PER_BLOCK + demo.valid + 2 * demo.satisfied;
        let gas_limit = executable as u64 * TRANSFER_GAS;
        let base_fees: Vec<u64> = (1..=self.blocks + 1)
            .map(|block| scheduled_base_fee(self.chain_spec, self.mode, block, gas_limit))
            .collect();
        let base_fee = |block: u64| base_fees[block as usize - 1];
        let store = selection::store();
        let mut db = InMemoryDB::default();
        let mut next_sender = self.population;
        let mut satisfied_so_far = 0;
        let mut built = Vec::with_capacity(self.blocks as usize);
        let mut slot_admitted = Vec::with_capacity(self.blocks as usize);
        let first_slot = Instant::now();
        let mut slot_end = first_slot;

        for block in 1..=self.blocks {
            if let Some(wait) = slot_end.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            slot_end = Instant::now() + DEMO_BLOCK_INTERVAL;
            slot_admitted.push(self.admitted.load(Ordering::Relaxed));

            let satisfied = next_sender..next_sender + demo.satisfied;
            let valid = satisfied.end..satisfied.end + demo.valid;
            next_sender = valid.end;
            let first_ordinary = (block as usize - 1) * FILLER_PER_BLOCK;
            let satisfiers = satisfied_so_far..satisfied_so_far + demo.satisfied;
            let satisfied_watched: Vec<usize> =
                satisfiers.clone().map(|target| demo.target_watched(target)).collect();
            for sender in satisfied.clone().chain(valid.clone()) {
                fund(&mut db, Accounts::Validity.address(sender));
            }
            for index in first_ordinary..first_ordinary + FILLER_PER_BLOCK {
                fund(&mut db, Accounts::Ordinary.address(index));
            }
            for index in satisfiers.clone() {
                fund(&mut db, Accounts::Satisfier.address(index));
            }

            let start = Instant::now();
            pool.fund_validity(satisfied.start..valid.end);
            let mut satisfied_hashes = Vec::with_capacity(demo.satisfied);
            pool.admit_validity(
                satisfied.clone(),
                |sender| vec![watched_balance_is_one(satisfied_watched[sender - satisfied.start])],
                |hash| satisfied_hashes.push(hash),
            );
            let mut valid_hashes = Vec::with_capacity(demo.valid);
            pool.admit_validity(valid.clone(), Demo::valid_predicates, |hash| {
                valid_hashes.push(hash)
            });
            let (ordinary, ordinary_senders) =
                pool.admit_ordinary(first_ordinary, FILLER_PER_BLOCK);
            let (satisfier_hashes, satisfier_senders) =
                pool.admit_satisfiers(satisfiers.start, &satisfied_watched);
            let prep = start.elapsed();

            let ctx = payload_context(self.chain_spec, self.mode, block, gas_limit);
            let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
            state
                .load_cache_account(Predeploys::L1_BLOCK_INFO)
                .expect("in-memory load cannot fail");
            let mut builder = ctx.block_builder(&mut state).expect("block builder");
            let attributes = ctx.best_transaction_attributes(builder.evm_mut().block());
            assert_eq!(
                attributes.basefee,
                base_fee(block),
                "block {block} builds at its scheduled fee"
            );
            let evaluated_before = metric(self.metrics, UNSATISFIED_EVALUATIONS);

            let parent = block_hash(block - 1);
            let pre_block_changes = TransitionState::default();
            let build_start = Instant::now();
            let dormancy_build =
                pool.pool.begin_dormancy_build(parent, block, Some(&pre_block_changes));
            let mut resting = (dormancy_build.is_none() && self.mode.resting().is_enabled())
                .then(|| selection::begin(&store, parent, block, &pre_block_changes));
            let carry = build_start.elapsed();
            let fallback = enforced && dormancy_build.is_none();
            let start = Instant::now();
            let best = pool.pool.best_transactions_with_attributes_and_parking(
                attributes,
                resting
                    .as_ref()
                    .filter(|_| self.mode.resting().is_enforced())
                    .map(|resting| resting.iterator_hold()),
                dormancy_build.as_ref(),
            );
            let construct = start.elapsed();
            let mut info = ExecutionInfo::default();
            let start = Instant::now();
            ctx.execute_best_transactions(
                &mut info,
                &mut builder,
                ParkableBestPayloadTransactions::new(best),
                resting.as_ref(),
            )
            .expect("in-memory execution cannot fail");
            let selection_time = start.elapsed();
            let start = Instant::now();
            if let Some(resting) = resting.take() {
                selection::put(&store, resting, block_hash(block));
            }
            if let Some(dormancy_build) = dormancy_build {
                dormancy_build.seal(block_hash(block));
            }
            let store_time = start.elapsed();

            let included = builder.executor().receipts().len();
            drop(builder);
            state.merge_transitions(BundleRetention::Reverts);
            let bundle = state.take_bundle();
            let committed = |address: Address| {
                bundle
                    .account(&address)
                    .and_then(|account| account.info.as_ref())
                    .is_some_and(|info| info.nonce == 1)
            };
            let validity_senders: Vec<Address> = satisfied
                .clone()
                .chain(valid.clone())
                .map(|sender| Accounts::Validity.address(sender))
                .collect();
            assert!(
                validity_senders.iter().all(|&sender| committed(sender)),
                "block {block} must include every valid and same-block-satisfied transaction"
            );
            assert!(
                ordinary_senders.iter().chain(&satisfier_senders).all(|&sender| committed(sender)),
                "block {block} must include all of its ordinary traffic"
            );
            assert_eq!(
                included, executable,
                "block {block} includes its executable transactions and no parked one"
            );
            assert_exhaustive_selection(self.metrics);
            let parked_evaluated = metric(self.metrics, UNSATISFIED_EVALUATIONS) - evaluated_before;
            let dormant = if enforced { dormant_gauge(self.metrics) } else { 0 };

            let senders: Vec<Address> = ordinary_senders
                .into_iter()
                .chain(satisfier_senders)
                .chain(validity_senders)
                .collect();
            // The next block builds on this one's nonces, so a mined transaction the lagging
            // maintenance has not yet removed fails its nonce rather than landing twice.
            for &sender in &senders {
                db.insert_account_info(
                    sender,
                    AccountInfo { balance: U256::from(u128::MAX), nonce: 1, ..Default::default() },
                );
            }
            self.canonical
                .send(CanonicalBlock {
                    number: block,
                    base_fee: base_fee(block),
                    pending_base_fee: base_fee(block + 1),
                    mined: [ordinary, satisfier_hashes, valid_hashes, satisfied_hashes].concat(),
                    senders,
                    bundle,
                })
                .expect("the maintenance thread outlives the builds");
            satisfied_so_far = satisfiers.end;
            let timing = BlockTiming {
                block,
                dormant,
                included,
                carry,
                construct,
                selection: selection_time,
                store: store_time,
                ..Default::default()
            };
            let overran = Instant::now() > slot_end;
            built.push((
                ConcurrentBlock {
                    timing,
                    prep,
                    parked_evaluated,
                    fallback,
                    overran,
                    ..Default::default()
                },
                build_start,
            ));
        }
        self.stop.store(true, Ordering::Relaxed);
        let admitted_at_stop = self.admitted.load(Ordering::Relaxed);
        let elapsed = first_slot.elapsed();
        slot_admitted.push(admitted_at_stop);
        for ((block, _), window) in built.iter_mut().zip(slot_admitted.windows(2)) {
            block.ingested = window[1] - window[0];
        }
        (built, admitted_at_stop - slot_admitted[0], elapsed)
    }
}

/// Runs the demo's blocks concurrently with ingest (see [`DemoConcurrency`]) on a pool whose
/// parked population [`run_demo`] admitted and classified, and reports every block and the run's
/// summary.
///
/// The build thread, the maintenance thread and the submitters share only the pool and the state
/// it validates against, so the build thread waits on the others only through the pool's own locks
/// (and the in-memory provider's, which validation and classification read); it never joins them
/// before the last block ends. Each block asserts that it includes every valid and same-block-
/// satisfied transaction and its ordinary traffic, and no parked transaction.
fn run_concurrent_demo(
    report: Report,
    metrics: &PrometheusHandle,
    matrix: &Matrix,
    chain_spec: &Arc<BaseChainSpec>,
    mode: Mode,
    population: usize,
    pool: &ProductionPool,
) {
    let concurrency = matrix.demo_concurrency.expect("the concurrent demo runs with submitters");
    let stop = AtomicBool::new(false);
    let admitted = AtomicUsize::new(0);
    let (canonical, maintenance_queue) = mpsc::channel();
    let (built, ingest, runs) = thread::scope(|scope| {
        let maintenance = scope.spawn(move || maintain(pool, maintenance_queue));
        let submitters: Vec<_> = (0..concurrency.submitters)
            .map(|submitter| {
                let (stop, admitted) = (&stop, &admitted);
                scope.spawn(move || {
                    submit_parked(pool, matrix.demo, concurrency, submitter, stop, admitted)
                })
            })
            .collect();
        let (built, admitted, elapsed) = ConcurrentBuilder {
            pool,
            metrics,
            chain_spec,
            mode,
            demo: matrix.demo,
            population,
            blocks: matrix.blocks,
            admitted: &admitted,
            stop: &stop,
            canonical,
        }
        .run();
        let batches = submitters
            .into_iter()
            .flat_map(|submitter| submitter.join().expect("a submitter panicked"))
            .collect();
        let runs = maintenance.join().expect("the maintenance thread panicked");
        (built, ConcurrentIngest { admitted, elapsed, batches, dormant_end: 0 }, runs)
    });
    let ingest = ConcurrentIngest {
        dormant_end: if mode.dormancy() == ValidityDormancyMode::Enforce {
            dormant_gauge(metrics)
        } else {
            0
        },
        ..ingest
    };
    assert_eq!(runs.len(), built.len(), "every sealed block is maintained");
    let blocks: Vec<ConcurrentBlock> = built
        .iter()
        .zip(&runs)
        .enumerate()
        .map(|(index, ((block, _), run))| {
            assert_eq!(
                run.block, block.timing.block,
                "maintenance follows the sealed blocks in order"
            );
            let next_build = built.get(index + 1).map(|(_, build_start)| *build_start);
            ConcurrentBlock {
                maintenance: run.finished - run.started,
                maintenance_lag: next_build
                    .map_or(Duration::ZERO, |next| run.finished.saturating_duration_since(next)),
                ..*block
            }
        })
        .collect();
    for block in &blocks {
        report.concurrent_row(mode, population, block);
    }
    report.concurrent_summary(mode, population, &concurrency, &ingest, &blocks, resident_bytes());
}

fn main() {
    let matrix = Matrix::from_env();
    let report = Report { builder: "native" };
    let chain_spec = chain_spec();
    let metrics = PrometheusBuilder::new().install_recorder().expect("installs the recorder");
    println!("blocks={} gap_ms={}", matrix.blocks, matrix.gap.map_or(0, |gap| gap.as_millis()));
    for &mode in &matrix.modes {
        for (case, populations) in &matrix.cases {
            for &population in populations {
                if *case == Case::Demo {
                    run_demo(report, &metrics, &matrix, &chain_spec, mode, population);
                    continue;
                }
                let timings =
                    run_case(report, &metrics, &matrix, &chain_spec, *case, mode, population);
                report.summary(*case, mode, population, &timings);
            }
        }
    }
}
