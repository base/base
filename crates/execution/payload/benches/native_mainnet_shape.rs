//! Per-block latency of the native (post-Denim) payload builder's selection path on a pool shaped
//! like Base mainnet, driven through the production `BaseTransactionPool` iterator.
//!
//! The shape comes from the mainnet sequencer's own per-block metrics: every block the pool holds
//! [`LIVE_RESTING`] validity transactions whose predicate stays unsatisfied, [`FRESH_PER_BLOCK`]
//! validity transactions arrive, are evaluated once, rest, and leave the pool before the next
//! block, and [`STANDARD_PER_BLOCK`] plain transfers are executed. Blocks are never gas-bound, so
//! every build drains the iterator, as the mainnet builder does.
//!
//! Each block runs the same calls as the native builder: take the resting set, carry it into the
//! block, snapshot the pool, run the selection loop, drop the iterator, then finish the block and
//! store the set. The table reports each phase and the whole build path.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_mainnet_shape
//! ```

use std::{
    sync::Arc,
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
    ParkableBestPayloadTransactions, RESTING_UNSEEN_RETAINED_BLOCKS, RestingPredicateStore,
    SharedRestingPredicates,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, GuardLimits, ParkableTransactionPool, ValidityOperator,
    ValidityPredicate,
};
use base_test_utils::build_test_genesis_everest;
use metrics_exporter_prometheus::PrometheusBuilder;
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::SealedHeader;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_revm::db::State;
use reth_tasks::Runtime;
use reth_transaction_pool::{
    BestTransactionsAttributes, Pool, PoolConfig, PoolTransaction, SubPoolLimit, TransactionOrigin,
    TransactionPool, blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};
use revm::{
    database::{InMemoryDB, TransitionState},
    state::AccountInfo,
};

type Client = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type BenchPool = BaseTransactionPool<Client, InMemoryBlobStore, BaseEvmConfig>;
type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

/// Validity transactions that stay in the pool with an unsatisfied predicate, as parked by the
/// mainnet builder in every block (`resting_predicate_parked_total` per block).
const LIVE_RESTING: usize = 38_500;
/// Validity transactions that arrive before a block, are evaluated once and leave the pool
/// (`validity_predicate_candidates_evaluated_per_build`, p50).
const FRESH_PER_BLOCK: usize = 1_300;
/// Plain transfers executed per block (`txs_included_per_block`, between p50 and p90).
const STANDARD_PER_BLOCK: usize = 25;
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
                BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
            let header = Header { timestamp: 1_700_000_000, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
            validator
        });
    let config = PoolConfig {
        pending_limit: SubPoolLimit::new(200_000, 250 * MIB),
        ..PoolConfig::default()
    };
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, config);
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

/// A signed transfer from a fresh funded sender. `watched` makes it a validity transaction whose
/// balance predicate on that address never holds.
fn transaction(client: &Client, tip: u128, watched: Option<Address>) -> BasePooledTransaction {
    let signer = PrivateKeySigner::random();
    client.add_account(signer.address(), ExtendedAccount::new(0, U256::from(ONE_ETHER)));
    let tx = TxEip1559 {
        chain_id: chain_id(),
        nonce: 0,
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
        builder_config: BaseBuilderConfig::default(),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

fn block_hash(block: u64) -> B256 {
    keccak256(block.to_be_bytes())
}

struct BlockTiming {
    carry: Duration,
    snapshot: Duration,
    selection: Duration,
    teardown: Duration,
    carried: usize,
}

impl BlockTiming {
    fn total(&self) -> Duration {
        self.carry + self.snapshot + self.selection + self.teardown
    }
}

fn hold_seconds(rendered: &str) -> f64 {
    rendered
        .lines()
        .find_map(|line| {
            line.strip_prefix("base_builder_resting_predicate_hold_duration_sum ")
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or_default()
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let recorder = PrometheusBuilder::new().build_recorder();
    let metrics_handle = recorder.handle();
    metrics::set_global_recorder(recorder).expect("no recorder installed yet");

    // The validation tasks run on the runtime the pool is created in.
    let (pool, client) = runtime.block_on(async { build_pool() });
    let ctx = payload_context();
    let store = RestingPredicateStore::new(Runtime::test());
    let mut db = InMemoryDB::default();

    let setup = Instant::now();
    runtime.block_on(async {
        let resting: Vec<_> = (0..LIVE_RESTING)
            .map(|index| transaction(&client, 10 + index as u128 % 50, Some(watched(index))))
            .collect();
        add(&pool, resting).await;
    });
    eprintln!("pool filled in {:.1}s", setup.elapsed().as_secs_f64());

    let mut next_watched = LIVE_RESTING;
    let mut timings = Vec::new();
    let mut hold_before_measure = 0.0;
    for block in 1..=WARMUP_BLOCKS + MEASURED_BLOCKS {
        // Untimed: this block's arrivals.
        let (fresh_hashes, standard_hashes) = runtime.block_on(async {
            let fresh: Vec<_> = (0..FRESH_PER_BLOCK)
                .map(|index| {
                    transaction(
                        &client,
                        10 + index as u128 % 50,
                        Some(watched(next_watched + index)),
                    )
                })
                .collect();
            let standard: Vec<_> = (0..STANDARD_PER_BLOCK)
                .map(|index| transaction(&client, 5 + index as u128, None))
                .collect();
            for transaction in &standard {
                db.insert_account_info(
                    transaction.sender(),
                    AccountInfo { balance: U256::from(ONE_ETHER), ..Default::default() },
                );
            }
            (add(&pool, fresh).await, add(&pool, standard).await)
        });
        next_watched += FRESH_PER_BLOCK;
        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let mut info = ExecutionInfo::default();
        if block == WARMUP_BLOCKS + 1 {
            hold_before_measure = hold_seconds(&metrics_handle.render());
        }

        let start = Instant::now();
        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
        resting.begin_block(block_hash(block - 1), block, Some(&TransitionState::default()));
        let carried = resting.len();
        let shared = SharedRestingPredicates::new(resting);
        let carry = start.elapsed();

        let best = ParkableBestPayloadTransactions::new(
            pool.best_transactions_with_attributes_and_parking(
                BestTransactionsAttributes::new(0, None),
                Some(shared.iterator_hold()),
                None,
            ),
        );
        let snapshot = start.elapsed() - carry;

        ctx.execute_best_transactions(&mut info, &mut builder, best, Some(&shared), None)
            .expect("in-memory execution cannot fail");
        let selection = start.elapsed() - carry - snapshot;

        let resting = shared.take_predicates();
        store.put(store.finish(resting), block_hash(block));
        let teardown = start.elapsed() - carry - snapshot - selection;

        assert_eq!(builder.executor().receipts().len(), STANDARD_PER_BLOCK);
        drop(builder);
        // Untimed: included and churned transactions leave the pool.
        pool.remove_transactions(fresh_hashes.into_iter().chain(standard_hashes).collect());

        if block > WARMUP_BLOCKS {
            timings.push(BlockTiming { carry, snapshot, selection, teardown, carried });
        }
    }
    let hold_ms = (hold_seconds(&metrics_handle.render()) - hold_before_measure) * 1e3
        / MEASURED_BLOCKS as f64;

    let ms = |duration: Duration| duration.as_secs_f64() * 1e3;
    let quantile = |values: &mut Vec<f64>, q: f64| {
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) as f64 * q).round() as usize]
    };
    let summarize = |label: &str, phase: &dyn Fn(&BlockTiming) -> Duration| {
        let mut values: Vec<f64> = timings.iter().map(|timing| ms(phase(timing))).collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        println!(
            "SUMMARY {label:<10} mean_ms={mean:>8.3} p50_ms={:>8.3} p90_ms={:>8.3} max_ms={:>8.3}",
            quantile(&mut values, 0.5),
            quantile(&mut values, 0.9),
            quantile(&mut values, 1.0),
        );
    };
    println!(
        "blocks={} carried_mean={} live_resting={LIVE_RESTING} fresh_per_block={FRESH_PER_BLOCK} standard_per_block={STANDARD_PER_BLOCK}",
        timings.len(),
        timings.iter().map(|timing| timing.carried).sum::<usize>() / timings.len(),
    );
    summarize("carry", &|timing| timing.carry);
    summarize("snapshot", &|timing| timing.snapshot);
    summarize("selection", &|timing| timing.selection);
    summarize("teardown", &|timing| timing.teardown);
    summarize("total", &BlockTiming::total);
    println!("SUMMARY hold       mean_ms={hold_ms:>8.3} (resting_predicate_hold_duration)");
}
