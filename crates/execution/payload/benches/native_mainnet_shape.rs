//! Per-block latency of the native (post-Denim) payload builder's selection path on a pool shaped
//! like Base mainnet, driven through the production `BaseTransactionPool` parking iterator.
//!
//! Every block the pool holds [`DEFAULT_RESTING`] validity transactions whose predicate stays
//! unsatisfied, [`DEFAULT_FRESH`] validity transactions arrive, are evaluated once, rest, and
//! leave the pool before the next block, and [`DEFAULT_STANDARD`] plain transfers are executed.
//! Blocks are never gas-bound, so every build drains the iterator.
//!
//! Each block runs the same calls as the native builder: take the resting set, carry it into the
//! block, snapshot the pool, run the selection loop, then finish the block and store the set. The
//! table reports the snapshot, the snapshot without the hold work it ran after releasing the
//! store locks (an upper bound on the time the lane stores' read guards were held), the selection
//! loop, how long the loop ran and how many validity transactions it held before the first
//! standard transaction was selected, and the time spent holding.
//!
//! The shape is configurable through environment variables:
//!
//! - `MAINNET_SHAPE_RESTING`: resting validity senders (default [`DEFAULT_RESTING`]).
//! - `MAINNET_SHAPE_LANE_DEPTH`: validity transactions per resting sender, in nonce order
//!   (default 1).
//! - `MAINNET_SHAPE_FRESH`: validity transactions arriving per block (default [`DEFAULT_FRESH`]).
//! - `MAINNET_SHAPE_STANDARD`: transfers per block (default [`DEFAULT_STANDARD`]).
//! - `MAINNET_SHAPE_TIPS`: `above` (default) prices every validity transaction above every
//!   transfer, `below` prices them below.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_mainnet_shape
//! ```

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::{BaseUpgrade, ChainConfig};
use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
    ParkableBestPayloadTransactions, RESTING_UNSEEN_RETAINED_BLOCKS, RestingPredicateMode,
    RestingPredicateStore, SharedRestingPredicates,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, BestTransactionsHold, GuardLimits, ParkableBestTransactions,
    ParkableTransactionPool, ValidityOperator, ValidityPredicate,
};
use base_test_utils::build_test_genesis_everest;
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::SealedHeader;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_revm::db::State;
use reth_tasks::Runtime;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, Pool, PoolConfig, PoolTransaction, SubPoolLimit,
    TransactionOrigin, TransactionPool, ValidPoolTransaction, blobstore::InMemoryBlobStore,
    error::InvalidPoolTransactionError, validate::EthTransactionValidatorBuilder,
};
use revm::{
    database::{InMemoryDB, TransitionState},
    state::AccountInfo,
};

type Client = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type BenchPool = BaseTransactionPool<Client, InMemoryBlobStore, BaseEvmConfig>;
type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

/// Validity transactions that stay in the pool with an unsatisfied predicate, as parked by the
/// mainnet builder in every block.
const DEFAULT_RESTING: usize = 38_500;
/// Validity transactions that arrive before a block, are evaluated once and leave the pool.
const DEFAULT_FRESH: usize = 1_300;
/// Plain transfers executed per block.
const DEFAULT_STANDARD: usize = 25;
/// Blocks built before measuring, so the resting set reaches its steady-state size.
const WARMUP_BLOCKS: u64 = 2 * RESTING_UNSEEN_RETAINED_BLOCKS;
/// Blocks measured.
const MEASURED_BLOCKS: u64 = 6 * RESTING_UNSEEN_RETAINED_BLOCKS;
/// Long enough that a block always waits for the previous block's sweep.
const SWEEP_WAIT_LIMIT: Duration = Duration::from_secs(1);
const TRANSFER_GAS: u64 = 21_000;
const DENIM_TIMESTAMP: u64 = 1;
const ONE_ETHER: u64 = 1_000_000_000_000_000_000;
const MIB: usize = 1024 * 1024;

const fn chain_id() -> u64 {
    ChainConfig::mainnet().chain_id
}

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| value.parse().expect("a number"))
}

/// Tip bands: validity transactions in one, transfers in the other.
struct Tips {
    validity_base: u128,
    standard_base: u128,
}

impl Tips {
    fn from_env() -> (Self, &'static str) {
        match std::env::var("MAINNET_SHAPE_TIPS").as_deref() {
            Ok("below") => (Self { validity_base: 10, standard_base: 10_000 }, "below"),
            _ => (Self { validity_base: 10_000, standard_base: 10 }, "above"),
        }
    }

    const fn validity(&self, index: usize) -> u128 {
        self.validity_base + (index % 50) as u128
    }

    const fn standard(&self, index: usize) -> u128 {
        self.standard_base + (index % 50) as u128
    }
}

fn build_pool() -> (BenchPool, Client) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = chain_id();
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let blob_store = InMemoryBlobStore::default();
    let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build_with_tasks(Runtime::test(), blob_store.clone())
        .map(|inner| {
            let validator =
                BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                    .require_l1_data_gas_fee(false);
            let header = Header { timestamp: 1_700_000_000, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, None);
            validator
        });
    let config = PoolConfig {
        pending_limit: SubPoolLimit::new(400_000, 500 * MIB),
        ..PoolConfig::default()
    };
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, config);
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

/// A signed transfer. `watched` makes it a validity transaction whose balance predicate on that
/// address never holds.
fn transaction(
    signer: &PrivateKeySigner,
    nonce: u64,
    tip: u128,
    watched: Option<Address>,
) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: chain_id(),
        nonce,
        gas_limit: TRANSFER_GAS,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: tip,
        to: TxKind::Call(Address::repeat_byte(0xee)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("signing cannot fail");
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    let recovered = envelope.clone().try_into_recovered().expect("signature recovers");
    let transaction = BasePooledTransaction::new(recovered, envelope.encode_2718_len());
    match watched {
        Some(address) => transaction.with_validity_predicates(vec![ValidityPredicate::Balance {
            address,
            op: ValidityOperator::Equal,
            value: U256::ONE,
        }]),
        None => transaction,
    }
}

fn funded_signer(client: &Client) -> PrivateKeySigner {
    let signer = PrivateKeySigner::random();
    client.add_account(signer.address(), ExtendedAccount::new(0, U256::from(ONE_ETHER)));
    signer
}

/// An address no transaction touches, so a balance predicate on it never changes.
fn watched(index: usize) -> Address {
    Address::from_word(keccak256(index.to_be_bytes()))
}

async fn add(pool: &BenchPool, transactions: Vec<BasePooledTransaction>) -> Vec<TxHash> {
    let mut hashes = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        hashes.push(
            pool.add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect("transaction is admitted")
                .hash,
        );
    }
    hashes
}

fn payload_context() -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    // Far above what a block uses, so every build drains the iterator.
    let gas_limit = 400_000_000;
    let parent = Arc::new(SealedHeader::seal_slow(Header { gas_limit, ..Default::default() }));
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
        evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig::default()
            .with_resting_predicate_mode(RestingPredicateMode::Enforce),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

fn block_hash(block: u64) -> B256 {
    keccak256(block.to_be_bytes())
}

/// What one build did before the selection loop received its first standard transaction.
#[derive(Default)]
struct Probe {
    /// Hold checks.
    holds: AtomicUsize,
    /// Hold checks that parked the transaction.
    held: AtomicUsize,
    hold_time: Mutex<Duration>,
    /// Of `hold_time`, the time recorded while the pool was snapshotted.
    snapshot_hold_time: Mutex<Duration>,
    /// Whether the pool is being snapshotted.
    in_snapshot: AtomicBool,
    /// Time since the selection loop started, and holds so far, at the first standard yield.
    first_standard: Mutex<Option<(Duration, usize)>>,
}

/// Counts the holds of the production hold it wraps.
struct CountingHold {
    inner: Box<dyn BestTransactionsHold<BasePooledTransaction>>,
    probe: Arc<Probe>,
}

impl BestTransactionsHold<BasePooledTransaction> for CountingHold {
    fn hold(&mut self, transaction: &ValidPoolTransaction<BasePooledTransaction>) -> bool {
        self.probe.holds.fetch_add(1, AtomicOrdering::Relaxed);
        let held = self.inner.hold(transaction);
        if held {
            self.probe.held.fetch_add(1, AtomicOrdering::Relaxed);
        }
        held
    }

    fn record_duration(&mut self, duration: Duration) {
        *self.probe.hold_time.lock().unwrap() += duration;
        if self.probe.in_snapshot.load(AtomicOrdering::Relaxed) {
            *self.probe.snapshot_hold_time.lock().unwrap() += duration;
        }
        self.inner.record_duration(duration);
    }

    fn is_interrupted(&self) -> bool {
        self.inner.is_interrupted()
    }
}

/// Records when the pool iterator first yields a standard transaction.
struct ProbedBest {
    inner: Box<dyn ParkableBestTransactions<BasePooledTransaction>>,
    probe: Arc<Probe>,
    start: Instant,
}

impl Iterator for ProbedBest {
    type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.inner.next();
        if let Some(transaction) = &next
            && transaction.transaction.validity_conditions().is_empty()
        {
            let mut first = self.probe.first_standard.lock().unwrap();
            if first.is_none() {
                *first =
                    Some((self.start.elapsed(), self.probe.holds.load(AtomicOrdering::Relaxed)));
            }
        }
        next
    }
}

impl BestTransactions for ProbedBest {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        self.inner.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.inner.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.inner.set_skip_blobs(skip_blobs);
    }
}

impl ParkableBestTransactions<BasePooledTransaction> for ProbedBest {
    fn park(&mut self, transaction: &Arc<ValidPoolTransaction<BasePooledTransaction>>) {
        self.inner.park(transaction);
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(
        &mut self,
        transaction_hash: TxHash,
        kind: InvalidPoolTransactionError,
    ) -> bool {
        self.inner.discard_parked(transaction_hash, kind)
    }

    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<BasePooledTransaction>>) {
        self.inner.mark_committed(transaction);
    }
}

struct BlockTiming {
    snapshot: Duration,
    snapshot_hold: Duration,
    selection: Duration,
    first_standard: Duration,
    holds_before_first_standard: usize,
    holds: usize,
    held: usize,
    hold_time: Duration,
    carried: usize,
}

fn main() {
    let resting_senders = env_or("MAINNET_SHAPE_RESTING", DEFAULT_RESTING);
    let lane_depth = env_or("MAINNET_SHAPE_LANE_DEPTH", 1).max(1);
    let fresh_per_block = env_or("MAINNET_SHAPE_FRESH", DEFAULT_FRESH);
    let standard_per_block = env_or("MAINNET_SHAPE_STANDARD", DEFAULT_STANDARD);
    let (tips, tips_label) = Tips::from_env();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    // The validation tasks run on the runtime the pool is created in.
    let (pool, client) = runtime.block_on(async { build_pool() });
    let ctx = payload_context();
    let store = RestingPredicateStore::new(Runtime::test());
    let mut db = InMemoryDB::default();

    let setup = Instant::now();
    runtime.block_on(async {
        let mut resting = Vec::with_capacity(resting_senders * lane_depth);
        for sender in 0..resting_senders {
            let signer = funded_signer(&client);
            for nonce in 0..lane_depth {
                let index = sender * lane_depth + nonce;
                resting.push(transaction(
                    &signer,
                    nonce as u64,
                    tips.validity(index),
                    Some(watched(index)),
                ));
            }
        }
        add(&pool, resting).await;
    });
    eprintln!("pool filled in {:.1}s", setup.elapsed().as_secs_f64());

    let mut next_watched = resting_senders * lane_depth;
    let mut timings = Vec::new();
    for block in 1..=WARMUP_BLOCKS + MEASURED_BLOCKS {
        // Untimed: this block's arrivals.
        let (fresh_hashes, standard_hashes) = runtime.block_on(async {
            let fresh: Vec<_> = (0..fresh_per_block)
                .map(|index| {
                    transaction(
                        &funded_signer(&client),
                        0,
                        tips.validity(index),
                        Some(watched(next_watched + index)),
                    )
                })
                .collect();
            let standard: Vec<_> = (0..standard_per_block)
                .map(|index| transaction(&funded_signer(&client), 0, tips.standard(index), None))
                .collect();
            for transaction in &standard {
                db.insert_account_info(
                    transaction.sender(),
                    AccountInfo { balance: U256::from(ONE_ETHER), ..Default::default() },
                );
            }
            (add(&pool, fresh).await, add(&pool, standard).await)
        });
        next_watched += fresh_per_block;
        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let mut info = ExecutionInfo::default();

        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
        resting.begin_block(block_hash(block - 1), block, Some(&TransitionState::default()));
        let carried = resting.len();
        let shared = SharedRestingPredicates::new(resting);
        let probe = Arc::new(Probe::default());
        let hold = CountingHold { inner: shared.iterator_hold(), probe: Arc::clone(&probe) };

        probe.in_snapshot.store(true, AtomicOrdering::Relaxed);
        let start = Instant::now();
        let inner = pool.best_transactions_with_attributes_and_parking(
            BestTransactionsAttributes::new(0, None),
            Some(Box::new(hold)),
        );
        let snapshot = start.elapsed();
        probe.in_snapshot.store(false, AtomicOrdering::Relaxed);

        let start = Instant::now();
        let best = ParkableBestPayloadTransactions::new(Box::new(ProbedBest {
            inner,
            probe: Arc::clone(&probe),
            start,
        }));
        ctx.execute_best_transactions(&mut info, &mut builder, best, Some(&shared))
            .expect("in-memory execution cannot fail");
        let selection = start.elapsed();

        let resting = shared.take_predicates();
        store.put(store.finish(resting), block_hash(block));

        assert_eq!(builder.executor().receipts().len(), standard_per_block);
        drop(builder);
        // Untimed: included and churned transactions leave the pool.
        pool.remove_transactions(fresh_hashes.into_iter().chain(standard_hashes).collect());

        if block > WARMUP_BLOCKS {
            let (first_standard, holds_before_first_standard) =
                probe.first_standard.lock().unwrap().unwrap_or((selection, 0));
            timings.push(BlockTiming {
                snapshot,
                snapshot_hold: *probe.snapshot_hold_time.lock().unwrap(),
                selection,
                first_standard,
                holds_before_first_standard,
                holds: probe.holds.load(AtomicOrdering::Relaxed),
                held: probe.held.load(AtomicOrdering::Relaxed),
                hold_time: *probe.hold_time.lock().unwrap(),
                carried,
            });
        }
    }

    let ms = |duration: Duration| duration.as_secs_f64() * 1e3;
    let quantile = |values: &mut Vec<f64>, q: f64| {
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) as f64 * q).round() as usize]
    };
    let summarize = |label: &str, unit: &str, value: &dyn Fn(&BlockTiming) -> f64| {
        let mut values: Vec<f64> = timings.iter().map(value).collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        println!(
            "SUMMARY {label:<22} mean_{unit}={mean:>10.3} p50_{unit}={:>10.3} p90_{unit}={:>10.3} max_{unit}={:>10.3}",
            quantile(&mut values, 0.5),
            quantile(&mut values, 0.9),
            quantile(&mut values, 1.0),
        );
    };
    println!(
        "blocks={} tips={tips_label} resting_senders={resting_senders} lane_depth={lane_depth} fresh_per_block={fresh_per_block} standard_per_block={standard_per_block} carried_mean={}",
        timings.len(),
        timings.iter().map(|timing| timing.carried).sum::<usize>() / timings.len(),
    );
    summarize("snapshot", "ms", &|timing| ms(timing.snapshot));
    // The snapshot without the hold work it ran after releasing the store locks: an upper bound
    // on the time the lane stores' read guards were held.
    summarize("snapshot_unheld", "ms", &|timing| ms(timing.snapshot - timing.snapshot_hold));
    summarize("selection", "ms", &|timing| ms(timing.selection));
    summarize("first_standard", "ms", &|timing| ms(timing.first_standard));
    summarize("holds_before_standard", "n", &|timing| timing.holds_before_first_standard as f64);
    summarize("holds", "n", &|timing| timing.holds as f64);
    summarize("held", "n", &|timing| timing.held as f64);
    summarize("hold_time", "ms", &|timing| ms(timing.hold_time));
}
