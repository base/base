//! Transaction insertion benchmarks through the production `BaseTransactionPool` wrapper.
//!
//! Compares exactly the call boundary changed by the shared-insertion-sender patch, excluding the
//! RPC hook's unchanged prep (EIP-8130 check, event emission, raw broadcast) from both routes:
//! - `direct`: `Pool::add_transaction` — the direct insertion call the pre-patch hook made
//! - `sender/N`: `EthApi::add_pool_transaction` — reth's batch sender, the adopted route, with
//!   `--txpool.max-batch-size` N
//!
//! Both run over the same `BasePoolBuilder`-style stack: `BaseTransactionValidator` inside reth's
//! pool inside `BaseTransactionPool`, with L1-data-fee enforcement disabled for mock state. State
//! is `MockEthProvider` (not MDBX) with no JSON-RPC/network/signature-recovery layers; signing and
//! encoding happen once outside all timed regions.
//!
//! Two measurement modes:
//! - Saturated (default): standard Criterion `iter_custom` sampling of whole-workload elapsed
//!   time. Criterion reports elapsed throughput, not per-transaction latencies.
//! - Fixed rate (`BASE_BENCH_RATE`, e.g. 5000 or 10000): absolute scheduled arrivals, independent
//!   of completions. Use `--test` for one workload per case and repeat externally. The concurrency
//!   cap delays API submission, not arrival timestamps: latency includes any virtual backlog.
//!   `FIXED_RATE` reports scheduled-arrival-to-completion p50/p95/p99, call-duration p99, and
//!   submit-delay p99 (generator/timer lateness plus waiting for the concurrency cap). These are
//!   different distributions; do not subtract their percentiles. They are not Criterion CIs.
//!
//! Reth's batch processor does not exit when its channel closes, so every repetition builds a
//! fresh pool plus `reth_tasks::Runtime` and tears them down with graceful shutdown before the
//! next iteration: no batch processors, validation workers, or pools survive across iterations.
//!
//! Environment:
//! - `BASE_BENCH_TXS`: submissions per iteration (default 131072; must be positive)
//! - `BASE_BENCH_CONCURRENCY`: bounded in-flight submissions (default 64; must be positive)
//! - `BASE_BENCH_RATE`: offered transactions/second for the open-loop mode (must be positive)

use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_common_rpc_types::BaseRpcTypes;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_rpc::{
    BaseEthApi, BaseTimeCache,
    eth::{receipt::BaseReceiptConverter, transaction::BaseTxInfoMapper},
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, BaseTransactionPool, BaseTransactionValidator,
};
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};
use futures::{
    stream,
    stream::{FuturesUnordered, StreamExt},
};
use reth_network_api::noop::NoopNetwork;
use reth_primitives_traits::Recovered;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_rpc::EthApi;
use reth_rpc_convert::RpcConverter;
use reth_rpc_eth_api::node::RpcNodeCoreAdapter;
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, SubPoolLimit, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};

type TestProvider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type TestEvmConfig = BaseEvmConfig<BaseChainSpec>;
type BenchPool = BaseTransactionPool<
    TestProvider,
    InMemoryBlobStore,
    TestEvmConfig,
    BasePooledTransaction,
    BaseOrdering<BasePooledTransaction>,
>;
type TestNode = RpcNodeCoreAdapter<TestProvider, BenchPool, NoopNetwork, TestEvmConfig>;
type TestRpcConvert = RpcConverter<
    BaseRpcTypes,
    TestEvmConfig,
    BaseReceiptConverter<TestProvider>,
    (),
    BaseTxInfoMapper<TestProvider>,
>;
type TestEthApi = BaseEthApi<TestNode, TestRpcConvert>;

const SIGNERS: usize = 64;
const FUNDED_BALANCE_WEI: u128 = 100_000_000_000_000_000_000; // 100 ETH
const RECIPIENT: Address = Address::new([0x42; 20]);

type Submission = Result<B256, String>;
type Job = Pin<Box<dyn Future<Output = Submission> + Send>>;

#[derive(Debug, Clone, Copy)]
enum Route {
    /// Pre-patch route: direct insertion on the pool.
    Direct,
    /// Adopted route: reth's batch sender with a given `--txpool.max-batch-size`.
    Sender,
}

struct BenchConfig {
    txs: usize,
    concurrency: usize,
    /// `BASE_BENCH_RATE`: `Some(rate)` selects the open-loop fixed-rate mode.
    rate: Option<u64>,
}

fn bench_config() -> &'static BenchConfig {
    static CONFIG: OnceLock<BenchConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let txs =
            std::env::var("BASE_BENCH_TXS").ok().and_then(|v| v.parse().ok()).unwrap_or(131_072);
        let concurrency =
            std::env::var("BASE_BENCH_CONCURRENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
        let rate = std::env::var("BASE_BENCH_RATE").ok().and_then(|v| v.parse().ok());
        assert!(txs > 0, "BASE_BENCH_TXS must be positive");
        assert!(concurrency > 0, "BASE_BENCH_CONCURRENCY must be positive");
        assert!(rate.is_none_or(|rate| rate > 0), "BASE_BENCH_RATE must be positive");
        BenchConfig { txs, concurrency, rate }
    })
}

fn deterministic_signer(i: usize) -> PrivateKeySigner {
    let mut bytes = [0u8; 32];
    bytes[0] = (i + 1) as u8;
    PrivateKeySigner::from_bytes(&bytes.into()).unwrap()
}

/// Builds the funded, signed, valid EIP-1559 transfer set once; the pool consumes clones.
fn prepared() -> &'static Vec<(BasePooledTransaction, Bytes)> {
    static PREPARED: OnceLock<Vec<(BasePooledTransaction, Bytes)>> = OnceLock::new();
    PREPARED.get_or_init(|| {
        let txs = bench_config().txs;
        let chain_id = ChainConfig::mainnet().chain_id;
        (0..txs)
            .map(|i| {
                let signer = deterministic_signer(i % SIGNERS);
                let nonce = (i / SIGNERS) as u64;
                let tx = TxEip1559 {
                    chain_id,
                    nonce,
                    max_priority_fee_per_gas: 1_000_000_000,
                    max_fee_per_gas: 10_000_000_000,
                    gas_limit: 21_000,
                    to: TxKind::Call(RECIPIENT),
                    value: U256::ZERO,
                    access_list: Default::default(),
                    input: Default::default(),
                };
                let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
                let signed: BaseTransactionSigned = tx.into_signed(signature).into();
                let recovered = Recovered::new_unchecked(signed, signer.address());
                let encoded = recovered.encoded_2718().into();
                let encoded_length = recovered.encode_2718_len();
                (BasePooledTransaction::new(recovered, encoded_length), encoded)
            })
            .collect()
    })
}

/// Mirrors `BasePoolBuilder::build_pool` wiring over mock state: the Base validator executor
/// inside reth's pool inside `BaseTransactionPool`, with `tasks` driving spawned validation and
/// batch-processor tasks so they can be shut down after each iteration.
fn bench_setup(max_batch_size: usize, tasks: Runtime) -> (TestEthApi, BenchPool) {
    let chain_spec = Arc::new(BaseChainSpec::mainnet());
    let provider = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    provider.extend_accounts(
        (0..SIGNERS)
            .map(|i| {
                let signer = deterministic_signer(i);
                (signer.address(), ExtendedAccount::new(0, U256::from(FUNDED_BALANCE_WEI)))
            })
            .collect::<alloy_primitives::map::AddressMap<ExtendedAccount>>(),
    );

    let validator = EthTransactionValidatorBuilder::new(provider.clone(), evm_config.clone())
        .build_with_tasks(tasks.clone(), InMemoryBlobStore::default())
        .map(|inner| BaseTransactionValidator::new(inner).require_l1_data_gas_fee(false));
    // Bench-only raised limits: defaults (10k txs per sub-pool, 16 account slots) cannot hold
    // the default prepared set (2,048 transactions per signer).
    let subpool = SubPoolLimit { max_txs: 200_000, max_size: 200 * 1024 * 1024 };
    let config = PoolConfig {
        pending_limit: subpool,
        basefee_limit: subpool,
        queued_limit: subpool,
        max_account_slots: 4096,
        ..PoolConfig::default()
    };
    let pool = BaseTransactionPool::new(
        Pool::new(validator, BaseOrdering::default(), InMemoryBlobStore::default(), config),
        BaseOrdering::default(),
    );

    let base_time = BaseTimeCache::default();
    let rpc_converter =
        RpcConverter::new(BaseReceiptConverter::new(provider.clone(), base_time.clone()))
            .with_mapper(BaseTxInfoMapper::new(provider.clone(), base_time.clone()));
    let eth_api = EthApi::builder(provider, pool.clone(), NoopNetwork::default(), evm_config)
        .with_rpc_converter(rpc_converter)
        .task_spawner(tasks)
        .max_batch_size(max_batch_size)
        .build_inner();
    (BaseEthApi::new(eth_api, None, U256::ZERO, base_time), pool)
}

fn submission_job(route: Route, api: &TestEthApi, tx: BasePooledTransaction) -> Job {
    // Identical harness ownership for both routes; borrow the underlying pool for direct calls.
    let api = api.clone();
    Box::pin(async move {
        match route {
            Route::Direct => api
                .eth_api()
                .pool()
                .add_transaction(TransactionOrigin::Local, tx)
                .await
                .map(|outcome| outcome.hash)
                .map_err(|err| err.to_string()),
            Route::Sender => api
                .eth_api()
                .add_pool_transaction(TransactionOrigin::Local, tx)
                .await
                .map(|outcome| outcome.hash)
                .map_err(|err| err.to_string()),
        }
    })
}

/// Tears down the shared task runtime so no batch processor, validation worker, or pool task
/// survives into the next iteration. Reth's batch processor parks forever on its channel, so the
/// graceful shutdown signal (not channel close) is what retires it before the runtime drops.
async fn shutdown_tasks(tasks: Runtime) {
    tasks.graceful_shutdown();
    if let Some(manager) = tasks.take_task_manager_handle() {
        manager.await.expect("task manager join failed").expect("critical task panicked");
    }
}

/// Runs one saturated workload in a fresh runtime/pool and returns only the timed submission
/// duration. Setup, validation asserts, and teardown happen outside the measured region.
async fn run_saturated(route: Route, max_batch_size: usize, config: &BenchConfig) -> Duration {
    let tasks = Runtime::test();
    let (api, pool) = bench_setup(max_batch_size, tasks.clone());
    let jobs: Vec<Job> = prepared()
        .iter()
        .take(config.txs)
        .cloned()
        .map(|tx| submission_job(route, &api, tx.0))
        .collect();

    let start = Instant::now();
    let results: Vec<Submission> =
        stream::iter(jobs).buffer_unordered(config.concurrency).collect().await;
    let elapsed = start.elapsed();

    for result in &results {
        assert!(result.is_ok(), "insertion failed: {:?}", result.as_ref().err());
    }
    assert_eq!(pool.pool_size().total, config.txs, "pool must retain every transaction");

    drop(results);
    drop(api);
    drop(pool);
    shutdown_tasks(tasks).await;
    elapsed
}

/// Absolute arrivals form a virtual queue: a concurrency cap delays submission, never the
/// scheduled arrival. Thus overload remains visible in latency and outstanding-at-end counts.
async fn run_fixed_rate(route: Route, max_batch_size: usize, config: &BenchConfig) -> Duration {
    let BenchConfig { txs, concurrency, rate } = *config;
    let rate = rate.expect("fixed-rate mode requires BASE_BENCH_RATE");
    let tasks = Runtime::test();
    let (api, pool) = bench_setup(max_batch_size, tasks.clone());
    let mut source = prepared().clone().into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut samples = Vec::with_capacity(txs);
    let mut submitted = 0;
    let start = Instant::now();
    let deadline = |i: usize| {
        start + Duration::from_nanos((i as u128 * 1_000_000_000 / u128::from(rate)) as u64)
    };
    let offered_end = deadline(txs);

    while samples.len() < txs {
        while submitted < txs
            && in_flight.len() < concurrency
            && deadline(submitted) <= Instant::now()
        {
            let scheduled = deadline(submitted);
            let tx = source.next().expect("one transaction per arrival");
            let job = submission_job(route, &api, tx.0);
            in_flight.push(async move {
                let called = Instant::now();
                let outcome = job.await;
                (scheduled, called, Instant::now(), outcome)
            });
            submitted += 1;
        }
        if submitted < txs && in_flight.len() < concurrency {
            tokio::select! {
                result = in_flight.next(), if !in_flight.is_empty() => {
                    samples.push(result.expect("nonempty in-flight set"));
                }
                _ = tokio::time::sleep_until(deadline(submitted).into()) => {}
            }
        } else {
            samples.push(in_flight.next().await.expect("outstanding submission"));
        }
    }
    let finished = samples.iter().map(|sample| sample.2).max().unwrap();
    let elapsed = finished.duration_since(start);
    let failures = samples.iter().filter(|sample| sample.3.is_err()).count();
    assert_eq!(failures, 0, "every submission must be accepted");
    assert_eq!(submitted, txs);
    assert_eq!(pool.pool_size().total, txs, "pool must retain every transaction");
    let outstanding = samples.iter().filter(|sample| sample.2 > offered_end).count();
    let mut latency = Vec::with_capacity(txs);
    let mut call = Vec::with_capacity(txs);
    let mut delay = Vec::with_capacity(txs);
    for (scheduled, called, completed, _) in samples {
        latency.push(completed.duration_since(scheduled).as_micros());
        call.push(completed.duration_since(called).as_micros());
        delay.push(called.duration_since(scheduled).as_micros());
    }
    latency.sort_unstable();
    call.sort_unstable();
    delay.sort_unstable();
    let pct = |values: &[u128], p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    println!(
        "FIXED_RATE route={route:?} batch={max_batch_size} offered={rate} achieved={:.1} n={txs} conc={concurrency} p50_us={} p95_us={} p99_us={} call_p99_us={} submit_delay_p99_us={} outstanding_at_end={outstanding} drain_ms={:.3} failures={failures}",
        txs as f64 / elapsed.as_secs_f64(),
        pct(&latency, 0.50),
        pct(&latency, 0.95),
        pct(&latency, 0.99),
        pct(&call, 0.99),
        pct(&delay, 0.99),
        finished.saturating_duration_since(offered_end).as_secs_f64() * 1000.0,
    );
    drop(api);
    drop(pool);
    shutdown_tasks(tasks).await;
    elapsed
}

fn bench_insertion(c: &mut Criterion) {
    let config: &'static BenchConfig = bench_config();
    // Prepare deterministic inputs before Criterion's warmup and measurement.
    let _ = prepared();

    let mut group = c.benchmark_group("insertion");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_millis(500));
    group.throughput(Throughput::Elements(config.txs as u64));

    let bench_case =
        |group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
         id: BenchmarkId,
         batch: usize| {
            group.bench_function(id, |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(4)
                            .enable_all()
                            .build()
                            .expect("bench tokio runtime should build");
                        let route = if batch == 0 { Route::Direct } else { Route::Sender };
                        total += rt.block_on(async {
                            if config.rate.is_some() {
                                run_fixed_rate(route, batch.max(1), config).await
                            } else {
                                run_saturated(route, batch.max(1), config).await
                            }
                        });
                        // Drop joins this iteration's async/blocking workers after their shutdown.
                        drop(rt);
                    }
                    total
                });
            });
        };

    bench_case(&mut group, BenchmarkId::new("direct", config.txs as u64), 0);
    for batch in [1usize, 8, 32, 128, 256, 512, 1024, 2048, 4096, 8192, 16384] {
        bench_case(&mut group, BenchmarkId::new("sender", batch as u64), batch);
    }

    group.finish();
}

criterion_group!(benches, bench_insertion);
criterion_main!(benches);
