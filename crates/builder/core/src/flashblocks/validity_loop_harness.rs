//! Measurement harness for the per-flashblock validity-candidate loop.
//!
//! Illustration only. Runs the real `execute_best_transactions` over a pending pool of `N`
//! parked validity candidates, once per simulated flashblock, plus staged component loops that
//! isolate pool iteration, predicate evaluation and predicate-index parking. The `Persist` stage
//! runs the same loop with [`PersistentValidityParking`] carried across flashblocks and blocks.
//!
//! Run (release, one process per emission mode because the event writer is a process global):
//! `VLH_EVENTS=1 VLH_OUT=/tmp/x cargo test --release -p base-builder-core --lib \
//!   flashblocks::validity_loop_harness -- --ignored --nocapture`
//!
//! `persistent_parking_matches_fresh_parking` is a regular test: it runs seeded multi-block
//! scenarios with executable transactions through both modes and asserts identical inclusion.

use std::{
    hint::black_box,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxHash, TxKind, U256, map::B256Set};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::RejectionCache;
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, PredicateContext,
    ValidityOperator, ValidityPredicate,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use reth_chainspec::ChainSpec;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_provider::noop::NoopProvider;
use reth_revm::{State, database::StateProviderDatabase};
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use revm::{
    Database, DatabaseCommit,
    bytecode::Bytecode,
    state::{Account, AccountInfo, EvmState, EvmStorageSlot},
};

use crate::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo, FlashblocksExtraCtx,
    ParkableBestPayloadTransactions, ParkablePayloadTransactions, ParkedPredicateIndex,
    PersistentValidityParking, PredicateReadRecorder, ResourceLimits, ValidityPredicateEvaluation,
};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type Txs = BestFlashblocksTxs<
    BasePooledTransaction,
    ParkableBestPayloadTransactions<BasePooledTransaction>,
>;

/// Transactions sharing one blocking storage key (a few to a few dozen per bucket).
const TXS_PER_BLOCKER: usize = 16;

fn address(index: usize) -> Address {
    Address::from_word(U256::from(index + 1).into())
}

/// Production-shaped predicate list: a satisfied block-number upper bound (required at ingress)
/// followed by an unsatisfied storage-slot equality on a key shared by `TXS_PER_BLOCKER` txs.
fn predicates(index: usize) -> Vec<ValidityPredicate> {
    let key = index / TXS_PER_BLOCKER;
    vec![
        ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(1_000_000u64),
        },
        ValidityPredicate::Storage {
            address: address(10_000_000 + key),
            slot: U256::from(key),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(1u64 + (index % 7) as u64),
        },
    ]
}

/// Builds a pooled EIP-1559 transaction from a unique sender with nonce 0.
fn pooled(
    sender: usize,
    to: Address,
    input: Bytes,
    tip: u128,
    predicates: Vec<ValidityPredicate>,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: 901,
        nonce: 0,
        gas_limit: 200_000,
        max_fee_per_gas: 10_000_000_000,
        max_priority_fee_per_gas: tip,
        to: TxKind::Call(to),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let mut transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), address(sender)),
        encoded_length,
    );
    if !predicates.is_empty() {
        transaction = transaction.with_validity_predicates(predicates);
    }
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new((sender as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// Parked candidate `index`; `generation` changes the hash, modelling a fee-bump replacement.
fn transaction(index: usize, generation: u32) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let mut input = vec![0xab; 68];
    input[..4].copy_from_slice(&generation.to_be_bytes());
    pooled(
        index,
        address(20_000_000 + index),
        Bytes::from(input),
        1_000_000 + index as u128,
        predicates(index),
    )
}

/// Builds the pool. `VLH_FRAG=1` interleaves each transaction allocation with live garbage
/// allocations and inserts in shuffled order so pool entries are scattered across the heap.
fn pool(n: usize, garbage: &mut Vec<Vec<u8>>) -> Pool {
    let frag = std::env::var("VLH_FRAG").is_ok_and(|v| v == "1");
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut order: Vec<usize> = (0..n).collect();
    if frag {
        // Deterministic Fisher-Yates shuffle.
        let mut x: u64 = 0x9e3779b97f4a7c15;
        for i in (1..n).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            order.swap(i, (x % (i as u64 + 1)) as usize);
        }
    }
    for index in order {
        if frag {
            for k in 0..8 {
                garbage.push(vec![k as u8; 64 + (index * 37 + k * 101) % 4096]);
            }
        }
        pool.add_transaction(transaction(index, 0), 0);
    }
    pool
}

/// Touches `VLH_THRASH_MB` of memory to evict CPU caches, approximating the other work a
/// busy node does between flashblock builds.
fn thrash(buffer: &mut [u8]) {
    let mut acc = 0u8;
    for i in (0..buffer.len()).step_by(64) {
        buffer[i] = buffer[i].wrapping_add(1);
        acc ^= buffer[i];
    }
    black_box(acc);
}

fn iterator(pool: &Pool, base_fee: u64) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    reth_transaction_pool::BestTransactions::no_updates(&mut best);
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        Ordering::coinbase_tip(),
        base_fee,
    )))
}

/// Builder context for block `number` (its parent is `number - 1`).
fn context_at(number: u64) -> BasePayloadBuilderCtx {
    let genesis: serde_json::Value = serde_json::json!({
        "config": { "chainId": 901 },
        "gasLimit": "0x1C9C380",
        "timestamp": "0x0"
    });
    let genesis = serde_json::from_value(genesis).expect("valid genesis");
    let inner = ChainSpec::builder().chain(901.into()).genesis(genesis).cancun_activated().build();
    let chain_spec = Arc::new(BaseChainSpec::from(inner));
    let parent_header = Header {
        number: number - 1,
        gas_limit: 60_000_000,
        timestamp: (number - 1) * 2,
        ..Default::default()
    };
    let parent = Arc::new(SealedHeader::seal_slow(parent_header));
    BasePayloadBuilderCtx::for_test(chain_spec, parent)
}

fn context() -> BasePayloadBuilderCtx {
    context_at(1)
}

fn quantile(samples: &mut [f64], q: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() - 1) as f64 * q).round() as usize]
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    /// refresh + next + park_current (BestFlashblocksTxs filters included).
    Iterate,
    /// Iterate + predicate evaluation through the recorder on the persistent State.
    Evaluate,
    /// Evaluate + predicate clone + ParkedPredicateIndex::park (fresh index per flashblock).
    Index,
    /// Real refresh + execute_best_transactions.
    Full,
    /// Full with persistent parking carried across flashblocks and blocks.
    Persist,
}

/// Flashblocks per simulated block (mainnet: 10 pool flashblocks per 2 s block).
const FLASHBLOCKS_PER_BLOCK: usize = 10;

struct RunSamples {
    fetch: Vec<f64>,
    work: Vec<f64>,
    /// `begin_block` (block-start flashblocks only) plus `begin_flashblock`, in µs.
    boundary_block_start: Vec<f64>,
    boundary_other: Vec<f64>,
    visited: u64,
    deferred: u64,
}

fn run(stage: Stage, n: usize, flashblocks: usize) -> RunSamples {
    let ctx = context();
    let base_fee = ctx.base_fee();
    let mut garbage = Vec::new();
    let mut pool = pool(n, &mut garbage);
    // `VLH_FRESH` replaces this many candidates per flashblock with new hashes, modelling
    // admissions and fee-bump replacements arriving between flashblocks.
    let fresh: usize = std::env::var("VLH_FRESH").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut txs: Vec<_> = (0..n).map(|index| transaction(index, 0)).collect();
    let mut generations = vec![0u32; n];
    let mut pooled: B256Set = txs.iter().map(|tx| *tx.hash()).collect();
    let mut cursor = 0usize;
    let thrash_mb: usize =
        std::env::var("VLH_THRASH_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut thrash_buffer = vec![0u8; thrash_mb * 1024 * 1024];
    let db = StateProviderDatabase::new(NoopProvider::default());
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let mut info = ExecutionInfo::default();
    let mut deferrals = BlockDeferrals::default();
    let limits = ResourceLimits { block_gas_limit: 60_000_000, ..Default::default() };
    // `VLH_REJ=1` uses a full 100k-entry, 30-minute-TTL rejection cache with churn.
    let rejection_churn = std::env::var("VLH_REJ").is_ok_and(|v| v == "1");
    let rejection_cache = RejectionCache::new(100_000, Duration::from_secs(1800));
    let mut next_rejected = 1u64 << 40;
    if rejection_churn {
        for _ in 0..100_000 {
            rejection_cache.insert(U256::from(next_rejected).into());
            next_rejected += 1;
        }
        rejection_cache.run_pending_tasks();
    }
    let mut best: Txs = BestFlashblocksTxs::new(iterator(&pool, base_fee), rejection_cache.clone());
    let mut parking =
        PersistentValidityParking::new(ctx.builder_config.predicate_bucket_ordered_threshold);
    let predicate_context = PredicateContext { block_number: 1, flashblock_index: 1 };
    let mut samples = RunSamples {
        fetch: Vec::with_capacity(flashblocks),
        work: Vec::with_capacity(flashblocks),
        boundary_block_start: Vec::new(),
        boundary_other: Vec::new(),
        visited: 0,
        deferred: 0,
    };
    for flashblock in 0..flashblocks {
        if thrash_mb > 0 {
            thrash(&mut thrash_buffer);
        }
        if rejection_churn {
            for _ in 0..50 {
                rejection_cache.insert(U256::from(next_rejected).into());
                next_rejected += 1;
            }
        }
        if fresh > 0 && flashblock > 0 {
            for _ in 0..fresh {
                let index = cursor % n;
                cursor += 1;
                generations[index] += 1;
                pooled.remove(txs[index].hash());
                txs[index] = transaction(index, generations[index]);
                pooled.insert(*txs[index].hash());
            }
            pool = PendingPool::new(Ordering::coinbase_tip());
            for tx in &txs {
                pool.add_transaction(Arc::clone(tx), 0);
            }
        }
        let block_start = flashblock % FLASHBLOCKS_PER_BLOCK == 0;
        if block_start {
            deferrals = BlockDeferrals::default();
        }
        let mut boundary_us = None;
        if matches!(stage, Stage::Persist) {
            let b0 = Instant::now();
            if block_start {
                parking.begin_block(&mut state, &predicate_context, |hash| pooled.contains(hash));
            }
            parking.begin_flashblock();
            boundary_us = Some(b0.elapsed().as_secs_f64() * 1e6);
        }
        let t0 = Instant::now();
        best.refresh_iterator(iterator(&pool, base_fee));
        let t1 = Instant::now();
        match stage {
            Stage::Full | Stage::Persist => {
                let persistent = matches!(stage, Stage::Persist).then_some(&mut parking);
                let diag = ctx
                    .execute_best_transactions_with_parking(
                        &mut info,
                        &mut deferrals,
                        &mut state,
                        &mut best,
                        &limits,
                        persistent,
                    )
                    .expect("selection succeeds");
                assert_eq!(diag.txs_considered as usize, n, "every candidate is visited");
                assert_eq!(diag.txs_included, 0, "no candidate is satisfiable");
                if matches!(stage, Stage::Full) {
                    assert_eq!(diag.txs_deferred as usize, n, "every candidate should park");
                }
                samples.visited = diag.txs_considered;
                if flashblock > 0 {
                    samples.deferred += diag.txs_deferred;
                }
            }
            Stage::Iterate | Stage::Evaluate | Stage::Index => {
                let mut index = ParkedPredicateIndex::new(
                    ctx.builder_config.predicate_bucket_ordered_threshold,
                );
                let mut count = 0;
                while let Some(tx) = best.next(()) {
                    count += 1;
                    let blocker = if matches!(stage, Stage::Iterate) {
                        Some(1)
                    } else {
                        let mut recorder =
                            PredicateReadRecorder::new(&mut state, &mut info.predicate_loads);
                        match ValidityPredicateEvaluation::evaluate(
                            tx.validity_predicates(),
                            &mut recorder,
                            &predicate_context,
                        )
                        .expect("noop reads")
                        {
                            ValidityPredicateEvaluation::Unsatisfied { blocker_index, .. } => {
                                Some(blocker_index)
                            }
                            ValidityPredicateEvaluation::Matched => None,
                        }
                    };
                    let blocker_index = blocker.expect("candidates are unsatisfied");
                    assert!(best.park_current());
                    if matches!(stage, Stage::Index) {
                        let predicate = tx.validity_predicates()[blocker_index].clone();
                        index.park(*tx.hash(), tx, predicate);
                    }
                }
                black_box(&index);
                samples.visited = count;
            }
        }
        let t2 = Instant::now();
        // The first flashblock starts with empty persistent parking; exclude it everywhere.
        if flashblock == 0 {
            continue;
        }
        samples.fetch.push((t1 - t0).as_secs_f64() * 1e6);
        samples.work.push((t2 - t1).as_secs_f64() * 1e6);
        if let Some(us) = boundary_us {
            if block_start {
                samples.boundary_block_start.push(us);
            } else {
                samples.boundary_other.push(us);
            }
        }
    }
    samples
}

#[test]
#[ignore = "local measurement harness"]
fn validity_loop_harness() {
    let events = std::env::var("VLH_EVENTS").is_ok_and(|v| v == "1");
    let out = PathBuf::from(std::env::var("VLH_OUT").unwrap_or_else(|_| "/tmp/vlh".into()));
    std::fs::create_dir_all(&out).expect("out dir");
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("install recorder");
    if events {
        let status = GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
            enabled: true,
            file_path: out.join("events.jsonl"),
            queue_capacity: base_observability_events::DEFAULT_QUEUE_CAPACITY,
            max_file_bytes: base_observability_events::DEFAULT_MAX_FILE_BYTES,
            max_files: 2,
            required: true,
            producer: TransactionEventProducer::BaseBuilder,
            network: "mainnet".to_string(),
        }))
        .expect("init writer");
        println!("event writer: {status:?}");
    }
    let flashblocks: usize =
        std::env::var("VLH_FLASHBLOCKS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let sizes: Vec<usize> = std::env::var("VLH_N")
        .unwrap_or_else(|_| "1000,4000,8000".into())
        .split(',')
        .map(|s| s.parse().expect("N"))
        .collect();
    let stages: Vec<Stage> = match std::env::var("VLH_STAGES").as_deref() {
        Ok("full") => vec![Stage::Full],
        Ok("persist") => vec![Stage::Full, Stage::Persist],
        _ if events => vec![Stage::Full, Stage::Persist],
        _ => vec![Stage::Iterate, Stage::Evaluate, Stage::Index, Stage::Full, Stage::Persist],
    };
    println!(
        "events,stage,n,visited,fetch_p50_us,work_p50_us,work_p90_us,work_per_candidate_p50_us,\
         work_per_candidate_p90_us,deferred_per_fb,boundary_block_start_p50_us,boundary_other_p50_us"
    );
    for &n in &sizes {
        for &stage in &stages {
            // Warm-up run then measured run.
            let _ = run(stage, n, 3);
            let mut samples = run(stage, n, flashblocks);
            let measured = samples.work.len() as f64;
            let f50 = quantile(&mut samples.fetch, 0.5);
            let w50 = quantile(&mut samples.work, 0.5);
            let w90 = quantile(&mut samples.work, 0.9);
            let bb = if samples.boundary_block_start.is_empty() {
                0.0
            } else {
                quantile(&mut samples.boundary_block_start, 0.5)
            };
            let bo = if samples.boundary_other.is_empty() {
                0.0
            } else {
                quantile(&mut samples.boundary_other, 0.5)
            };
            println!(
                "{},{stage:?},{n},{},{f50:.1},{w50:.1},{w90:.1},{:.3},{:.3},{:.1},{bb:.1},{bo:.1}",
                u8::from(events),
                samples.visited,
                w50 / n as f64,
                w90 / n as f64,
                samples.deferred as f64 / measured,
            );
        }
    }
    let _ = handle.render().len();
}

/// Writer contract: stores the first calldata word into slot 0.
const WRITER_CODE: [u8; 7] = [0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00];
const WRITERS: usize = 4;
const BLOCKS: u64 = 4;
const FLASHBLOCKS: u64 = 5;

fn writer(index: usize) -> Address {
    address(30_000_000 + index)
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }

    fn operator(&mut self) -> ValidityOperator {
        [
            ValidityOperator::Equal,
            ValidityOperator::NotEqual,
            ValidityOperator::LessThan,
            ValidityOperator::LessThanOrEqual,
            ValidityOperator::GreaterThan,
            ValidityOperator::GreaterThanOrEqual,
        ][self.below(6) as usize]
    }
}

/// One seeded scenario: transaction arrivals per (block, flashblock) and the external
/// (deposit-like) slot writes applied at each block start.
struct Scenario {
    arrivals: Vec<(u64, u64, Arc<ValidPoolTransaction<BasePooledTransaction>>)>,
    external_writes: Vec<(u64, usize, u64)>,
    validity: B256Set,
}

impl Scenario {
    fn new(seed: u64) -> Self {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut arrivals = Vec::new();
        let mut validity = B256Set::default();
        let mut sender = 0usize;
        for block in 1..=BLOCKS {
            for flashblock in 1..=FLASHBLOCKS {
                for _ in 0..rng.below(4) {
                    let value = U256::from(rng.below(4));
                    let tx = pooled(
                        sender,
                        writer(rng.below(WRITERS as u64) as usize),
                        Bytes::from(value.to_be_bytes::<32>().to_vec()),
                        1_000_000 + rng.below(1_000) as u128 * 1_000,
                        Vec::new(),
                    );
                    sender += 1;
                    arrivals.push((block, flashblock, tx));
                }
                for _ in 0..rng.below(8) {
                    let mut predicates = vec![ValidityPredicate::Storage {
                        address: writer(rng.below(WRITERS as u64) as usize),
                        slot: U256::ZERO,
                        mask: U256::MAX,
                        op: rng.operator(),
                        value: U256::from(rng.below(4)),
                    }];
                    if rng.below(3) == 0 {
                        predicates.push(ValidityPredicate::Storage {
                            address: writer(rng.below(WRITERS as u64) as usize),
                            slot: U256::ZERO,
                            mask: U256::MAX,
                            op: rng.operator(),
                            value: U256::from(rng.below(4)),
                        });
                    }
                    if rng.below(3) == 0 {
                        predicates.push(ValidityPredicate::BlockNumber {
                            op: ValidityOperator::LessThanOrEqual,
                            value: U256::from(block + rng.below(2)),
                        });
                    }
                    if rng.below(4) == 0 {
                        let op = if rng.below(2) == 0 {
                            ValidityOperator::GreaterThanOrEqual
                        } else {
                            ValidityOperator::LessThanOrEqual
                        };
                        predicates.push(ValidityPredicate::FlashblockIndex {
                            op,
                            value: U256::from(1 + rng.below(FLASHBLOCKS)),
                        });
                    }
                    if rng.below(2) == 0 {
                        predicates.reverse();
                    }
                    let tx = pooled(
                        sender,
                        address(20_000_000 + sender),
                        Bytes::new(),
                        1_000_000 + rng.below(1_000) as u128 * 1_000,
                        predicates,
                    );
                    sender += 1;
                    validity.insert(*tx.hash());
                    arrivals.push((block, flashblock, tx));
                }
            }
        }
        let external_writes = (2..=BLOCKS)
            .map(|block| (block, rng.below(WRITERS as u64) as usize, rng.below(4)))
            .collect();
        Self { arrivals, external_writes, validity }
    }
}

/// Per-flashblock outcome: (block, flashblock, included hashes, evicted hashes, considered).
type Outcome = (u64, u64, Vec<TxHash>, Vec<TxHash>, u64);

/// Applies a storage write outside the selection loop, like a deposit at block start.
fn external_write(
    state: &mut State<StateProviderDatabase<NoopProvider>>,
    contract: Address,
    value: u64,
) {
    let info = state.basic(contract).expect("read").expect("writer exists");
    let original = state.storage(contract, U256::ZERO).expect("read");
    let mut account = Account::from(info);
    account.mark_touch();
    account.storage.insert(
        U256::ZERO,
        EvmStorageSlot::new_changed(original, U256::from(value), Default::default()),
    );
    state.commit(EvmState::from_iter([(contract, account)]));
}

fn simulate(scenario: &Scenario, persist: bool, ordered_threshold: usize) -> (Vec<Outcome>, u64) {
    let db = StateProviderDatabase::new(NoopProvider::default());
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    for (_, _, tx) in &scenario.arrivals {
        state.insert_account(
            tx.sender(),
            AccountInfo { balance: U256::from(10u128.pow(20)), ..Default::default() },
        );
    }
    for index in 0..WRITERS {
        state.insert_account(
            writer(index),
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(&WRITER_CODE))),
        );
    }
    let limits = ResourceLimits { block_gas_limit: 60_000_000, ..Default::default() };
    let rejection_cache = RejectionCache::new(1_000, Duration::from_secs(60));
    let mut parking = PersistentValidityParking::new(ordered_threshold);
    let mut live: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>> = Vec::new();
    let mut outcomes = Vec::new();
    let mut deferred = 0;
    for block in 1..=BLOCKS {
        for (_, contract, value) in scenario.external_writes.iter().filter(|w| w.0 == block) {
            external_write(&mut state, writer(*contract), *value);
        }
        let mut info = ExecutionInfo::default();
        let mut deferrals = BlockDeferrals::default();
        let empty = PendingPool::new(Ordering::coinbase_tip());
        let mut best: Txs = BestFlashblocksTxs::new(
            iterator(&empty, context_at(block).base_fee()),
            rejection_cache.clone(),
        );
        for flashblock in 1..=FLASHBLOCKS {
            live.extend(
                scenario
                    .arrivals
                    .iter()
                    .filter(|(b, f, _)| *b == block && *f == flashblock)
                    .map(|(_, _, tx)| Arc::clone(tx)),
            );
            let mut ctx = context_at(block).with_extra_ctx(FlashblocksExtraCtx {
                flashblock_index: flashblock,
                ..Default::default()
            });
            ctx.builder_config.predicate_bucket_ordered_threshold = ordered_threshold;
            let mutate = std::env::var("VLH_MUTATE").unwrap_or_default();
            if persist {
                if flashblock == 1 && mutate != "no_block" {
                    let pooled: B256Set = live.iter().map(|tx| *tx.hash()).collect();
                    parking.begin_block(
                        &mut state,
                        &PredicateContext { block_number: block, flashblock_index: flashblock },
                        |hash| pooled.contains(hash),
                    );
                }
                if mutate != "no_flashblock" {
                    parking.begin_flashblock();
                }
            }
            let mut pool = PendingPool::new(Ordering::coinbase_tip());
            for tx in &live {
                pool.add_transaction(Arc::clone(tx), 0);
            }
            best.refresh_iterator(iterator(&pool, ctx.base_fee()));
            let before = info.executed_transactions.len();
            let diag = ctx
                .execute_best_transactions_with_parking(
                    &mut info,
                    &mut deferrals,
                    &mut state,
                    &mut best,
                    &limits,
                    persist.then_some(&mut parking),
                )
                .expect("selection succeeds");
            let included: Vec<TxHash> = info.executed_transactions[before..]
                .iter()
                .map(|tx| TxHash::from(*tx.tx_hash()))
                .collect();
            let evicted = diag.permanently_rejected_txs.clone();
            best.mark_committed(&included);
            best.mark_rejected(&evicted);
            live.retain(|tx| !included.contains(tx.hash()) && !evicted.contains(tx.hash()));
            deferred += diag.txs_deferred;
            outcomes.push((block, flashblock, included, evicted, diag.txs_considered));
        }
    }
    (outcomes, deferred)
}

#[test]
fn persistent_parking_matches_fresh_parking() {
    let mut validity_included = 0;
    let mut fresh_deferred = 0;
    let mut persist_deferred = 0;
    for seed in 1..=200u64 {
        let scenario = Scenario::new(seed);
        for threshold in [1, 32] {
            let (fresh, fresh_defers) = simulate(&scenario, false, threshold);
            let (persisted, persist_defers) = simulate(&scenario, true, threshold);
            assert_eq!(fresh, persisted, "seed {seed} threshold {threshold}");
            fresh_deferred += fresh_defers;
            persist_deferred += persist_defers;
            validity_included += fresh
                .iter()
                .flat_map(|outcome| &outcome.2)
                .filter(|hash| scenario.validity.contains(*hash))
                .count();
        }
    }
    println!(
        "validity included {validity_included}, deferrals fresh {fresh_deferred} persist {persist_deferred}"
    );
    assert!(validity_included > 0, "scenarios must include validity transactions");
    assert!(persist_deferred < fresh_deferred, "persistent parking must skip re-evaluation");
}
