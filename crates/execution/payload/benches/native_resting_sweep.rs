//! Per-block cost of the native (post-Denim) payload builder selecting from the production Base
//! transaction pool while it holds a resting validity population.
//!
//! The pool is the production `BaseTransactionPool` (see `fixtures/production_pool.rs`). It
//! holds a population of high-priority validity transactions whose predicates stay unsatisfied,
//! plus [`FILLER_PER_BLOCK`] fresh ordinary transfers admitted before every block. Each block
//! runs the native builder's calls in order: carry the builder's resting set into the block when
//! its resting mode is on, report the pre-selection writes to the pool, build the pool iterator,
//! run the selection loop, report the validity transactions it found unsatisfied, store the
//! resting set, and report the sealed block's state. Between blocks the pool mines the previous
//! block through the node's canonical update, which also moves the base fee, and runs the node's
//! canonical state-diff maintenance with that block's bundle, which advances validity dormancy.
//! The matrix of populations, lane kinds, liveness cases and modes, the phases each block is split
//! into, and the environment that selects them are described in `fixtures/resting_matrix.rs`.
//!
//! Every block asserts that it includes all of its ordinary traffic and nothing else, plus the
//! woken lanes in the liveness case's wake block. When a mode withholds resting transactions, the
//! last block's probe must yield nothing but executable transactions, so the population is
//! withheld from the iterator's input rather than yielded and skipped; under pool dormancy the
//! pool's dormant gauge must also count every lane still resting. The liveness case also asserts
//! that the pool expired its expiring lanes.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! RESTING_MATRIX_CASES=protocol RESTING_MATRIX_POPULATIONS=75000 \
//!   RESTING_MATRIX_MODES=enforce,off,builder_enforce \
//!   cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! ```

#[path = "fixtures/production_pool.rs"]
pub mod production_pool;
#[path = "fixtures/resting_matrix.rs"]
pub mod resting_matrix;

use std::{sync::Arc, time::Instant};

use alloy_consensus::Header;
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
use base_execution_txpool::{BuiltState, ParkableTransactionPool, ValidityDormancyMode};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use production_pool::{Accounts, ProductionPool, TRANSFER_GAS};
use resting_matrix::{
    BlockTiming, Case, Matrix, Mode, Phase, Probe, Report, WOKEN, block_hash, fund_woken,
    parent_base_fee, timed_drop, wake_transitions,
};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::SealedHeader;
use reth_revm::db::State;
use revm::{
    database::{BundleState, InMemoryDB, states::bundle_state::BundleRetention},
    state::AccountInfo,
};

type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

const CHAIN_ID: u64 = 8_453;
/// Ordinary transfers admitted before, and included in, each block.
const FILLER_PER_BLOCK: usize = 120;
const DENIM_TIMESTAMP: u64 = 1;
/// The pool's dormant-transaction gauge as Prometheus renders it.
const DORMANT_GAUGE: &str = "txpool_validity_dormant_transactions";

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

/// A Denim payload context for `block`, whose parent carries the block's scheduled base fee and
/// whose gas limit fits a block of filler plus every woken lane.
fn payload_context(chain_spec: &Arc<BaseChainSpec>, mode: Mode, block: u64) -> Ctx {
    let gas_limit = (FILLER_PER_BLOCK + WOKEN) as u64 * TRANSFER_GAS;
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
        builder_config: BaseBuilderConfig::default().with_resting_predicate_mode(mode.resting()),
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

/// The pool's dormant-transaction gauge, zero before the pool first sets it.
fn dormant_gauge(metrics: &PrometheusHandle) -> usize {
    metrics
        .render()
        .lines()
        .find_map(|line| line.strip_prefix(DORMANT_GAUGE)?.trim().parse::<f64>().ok())
        .map_or(0, |value| value as usize)
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
    let lanes = case.lanes(population);
    let start = Instant::now();
    let (resting_hashes, resting_senders) =
        pool.admit_resting(case.kind(), lanes, |lane| case.predicates(lane));
    report.setup(case, mode, population, lanes, start.elapsed());

    let mut db = InMemoryDB::default();
    for sender in &resting_senders {
        fund(&mut db, *sender);
    }
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

        let ctx = payload_context(chain_spec, mode, block);
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
            mutation = start.elapsed();
            let start = Instant::now();
            pool.maintain_canonical(block - 1, &bundle);
            dormancy = start.elapsed();
        }
        let start = Instant::now();
        let (ordinary, ordinary_senders) = pool.admit_ordinary(first_ordinary, FILLER_PER_BLOCK);
        mutation += start.elapsed();
        let held_drop = held.take().map(timed_drop).unwrap_or_default();
        if case.expired_by() == Some(block) {
            for lane in case.expiring() {
                assert!(
                    !pool.contains(&resting_hashes[lane]),
                    "the pool must expire lane {lane} after its deadline"
                );
            }
        }

        let pre_block_changes = wake_transitions(woken.clone());
        let start = Instant::now();
        let mut resting = mode
            .resting()
            .is_enabled()
            .then(|| selection::begin(&store, block_hash(block - 1), block, &pre_block_changes));
        let carry = start.elapsed();
        let members = resting.as_ref().map_or(0, |resting| resting.len());

        let start = Instant::now();
        pool.pool.report_built_state(block, BuiltState::Transitions(&pre_block_changes));
        let mut report_time = start.elapsed();
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
        let best = pool.pool.best_transactions_with_attributes_and_parking(attributes, hold());
        let construct = start.elapsed();
        let executable = FILLER_PER_BLOCK + woken.len();
        let probe = Probe::run(|| {
            pool.pool.best_transactions_with_attributes_and_parking(attributes, hold())
        });
        let phase = Phase::of(mode, probe.yielded, executable, !woken.is_empty());
        let probe_drop = if case.holds_iterator() {
            held = Some(probe.iterator);
            Default::default()
        } else {
            timed_drop(probe.iterator)
        };

        let mut info = ExecutionInfo {
            unsatisfied_validity_txs: pool.pool.collects_unsatisfied().then(Vec::new),
            ..Default::default()
        };
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
        assert_eq!(included, executable, "block {block} must include every executable transaction");

        let start = Instant::now();
        if let Some(unsatisfied) = info.unsatisfied_validity_txs.take()
            && !unsatisfied.is_empty()
        {
            pool.pool.report_unsatisfied(unsatisfied);
        }
        report_time += start.elapsed();

        let start = Instant::now();
        if let Some(resting) = resting.take() {
            selection::put(&store, resting, block_hash(block));
        }
        let store_time = start.elapsed();

        drop(builder);
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let start = Instant::now();
        pool.pool.report_built_state(block, BuiltState::Bundle(&bundle));
        report_time += start.elapsed();

        mined = (ordinary, ordinary_senders, bundle);
        for lane in woken {
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
            report: report_time,
            ..Default::default()
        };
        report.row(case, mode, population, &timing);
        timings.push(timing);
        if let Some(gap) = matrix.gap {
            std::thread::sleep(gap);
        }
    }

    let last = timings.last().expect("the matrix builds blocks");
    if mode.withholds() && lanes > 0 {
        assert_eq!(
            last.phase,
            Phase::Steady,
            "the resting population must be withheld from the last block's iterator"
        );
    }
    if mode.dormancy() == ValidityDormancyMode::Enforce && lanes > 0 {
        assert_eq!(
            last.dormant,
            case.final_members(lanes),
            "dormant validity transactions at the last block"
        );
    }
    timings
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
                let timings =
                    run_case(report, &metrics, &matrix, &chain_spec, *case, mode, population);
                report.summary(*case, mode, population, &timings);
            }
        }
    }
}
