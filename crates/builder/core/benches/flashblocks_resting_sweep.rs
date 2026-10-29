//! Per-block latency of the flashblocks payload builder carrying a large resting set, including
//! the blocks that sweep members no build yielded.
//!
//! The pool holds [`LIVE_RESTING`] validity transactions whose predicates never become satisfied,
//! so every flashblock yields them and holds them back, plus enough plain transfers to fill every
//! flashblock. Each block also rests [`CHURN_PER_BLOCK`] members whose transactions then leave the
//! pool, so the sweep always has members to drop and the set stays at a steady size.
//!
//! Blocks run back to back through the same store calls as the flashblocks builder: take the set
//! and carry it into the block, run [`FLASHBLOCKS`] flashblocks of the selection loop, then finish
//! the block and store the set. By default the next block starts immediately, without the state
//! root and leeway the builder overlaps an end-of-block sweep with, so a sweep still running is
//! counted against the next block's carry. `RESTING_SWEEP_BENCH_GAP_MS` adds an untimed pause
//! between blocks to model them.
//!
//! The carry column is the work between the forkchoice update and the first flashblock. Before
//! the sweep moved off the block-start path, the builder ran it before deriving the flashblock
//! schedule, so every millisecond of carry was time drift: at a 170ms flashblock interval, a
//! 170ms carry costs a flashblock.
//!
//! ```sh
//! cargo bench -p base-builder-core --bench flashblocks_resting_sweep
//! ```

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
    ParkableBestPayloadTransactions, RejectionCache, ResourceLimits, RestingPredicateMode,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, RESTING_UNSEEN_RETAINED_BLOCKS,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use reth_chainspec::ChainSpec;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::State;
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use revm::{
    database::{InMemoryDB, TransitionState},
    state::AccountInfo,
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type BestTxs = BestFlashblocksTxs<
    BasePooledTransaction,
    ParkableBestPayloadTransactions<BasePooledTransaction>,
>;

const CHAIN_ID: u64 = 901;
/// Resting validity transactions the pool yields in every flashblock.
const LIVE_RESTING: usize = 100_000;
/// Members rested in each block whose transactions leave the pool afterwards.
const CHURN_PER_BLOCK: u64 = 4_800;
/// Flashblocks per block.
const FLASHBLOCKS: usize = 10;
/// Plain transfers that fill each flashblock.
const FILLER_PER_FLASHBLOCK: usize = 100;
/// Blocks built before measuring, so the set reaches its steady-state size.
const WARMUP_BLOCKS: u64 = 3 * RESTING_UNSEEN_RETAINED_BLOCKS;
/// Blocks measured.
const MEASURED_BLOCKS: u64 = 4 * RESTING_UNSEEN_RETAINED_BLOCKS;
const TRANSFER_GAS: u64 = 21_000;
const SENDER_OFFSET: usize = 1_000_000;

fn address(index: usize) -> Address {
    Address::from_word(U256::from(index + 1).into())
}

fn transaction(
    sender_index: usize,
    priority_fee: u128,
    predicates: Vec<ValidityPredicate>,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: TRANSFER_GAS,
        max_fee_per_gas: 1_000_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(Address::repeat_byte(0xee)),
        value: U256::from(sender_index),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let sender = address(SENDER_OFFSET + sender_index);
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
        encoded_length,
    )
    .with_validity_predicates(predicates);
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(((SENDER_OFFSET + sender_index) as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// [`LIVE_RESTING`] never-satisfied validity transactions, each watching its own balance, plus
/// filler for every flashblock, and the parent state that funds every sender.
fn resting_pool() -> (Pool, InMemoryDB) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    for sender_index in 0..LIVE_RESTING + FLASHBLOCKS * FILLER_PER_FLASHBLOCK {
        let (priority_fee, predicates) = if sender_index < LIVE_RESTING {
            (
                10,
                vec![ValidityPredicate::Balance {
                    address: address(sender_index),
                    op: ValidityOperator::Equal,
                    value: U256::ONE,
                }],
            )
        } else {
            (1, Vec::new())
        };
        pool.add_transaction(transaction(sender_index, priority_fee, predicates), 0);
        db.insert_account_info(
            address(SENDER_OFFSET + sender_index),
            AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
        );
    }
    (pool, db)
}

fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    best.no_updates();
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        Ordering::coinbase_tip(),
        0,
    )))
}

fn payload_context() -> BasePayloadBuilderCtx {
    let genesis = serde_json::from_value(serde_json::json!({
        "config": { "chainId": CHAIN_ID },
        "gasLimit": "0x1C9C380",
        "timestamp": "0x0"
    }))
    .expect("valid genesis");
    let chain_spec =
        ChainSpec::builder().chain(CHAIN_ID.into()).genesis(genesis).cancun_activated().build();
    let parent = Header { gas_limit: 30_000_000, timestamp: 0, ..Default::default() };
    BasePayloadBuilderCtx::for_test(
        Arc::new(BaseChainSpec::from(chain_spec)),
        Arc::new(SealedHeader::seal_slow(parent)),
    )
}

/// Runs every flashblock of one block through the selection loop.
fn run_flashblocks(
    ctx: &BasePayloadBuilderCtx,
    pool: &Pool,
    state: &mut State<InMemoryDB>,
    best: &mut BestTxs,
) -> usize {
    let mut info = ExecutionInfo::default();
    let mut deferrals = BlockDeferrals::default();
    let mut committed_until = 0;
    for flashblock in 0..FLASHBLOCKS {
        if flashblock > 0 {
            best.refresh_iterator(parkable(pool));
        }
        let limits = ResourceLimits {
            block_gas_limit: ((flashblock + 1) * FILLER_PER_FLASHBLOCK) as u64 * TRANSFER_GAS,
            ..Default::default()
        };
        ctx.execute_best_transactions(&mut info, &mut deferrals, state, best, &limits)
            .expect("in-memory execution cannot fail");
        let committed = info.executed_transactions[committed_until..]
            .iter()
            .map(|transaction| transaction.tx_hash())
            .collect::<Vec<_>>();
        best.mark_committed(&committed);
        committed_until = info.executed_transactions.len();
    }
    info.executed_transactions.len()
}

/// A storage predicate on its own slot, the shape of a smart-account nonce check.
fn churn_predicate(index: u64) -> ValidityPredicate {
    ValidityPredicate::Storage {
        address: Address::repeat_byte(0xcc),
        slot: U256::from(index),
        mask: U256::MAX,
        op: ValidityOperator::Equal,
        value: U256::from(index + 1),
    }
}

fn block_hash(block: u64) -> B256 {
    keccak256(block.to_be_bytes())
}

/// Store calls that differ between builder versions.
mod store {
    use alloy_primitives::B256;
    use base_execution_payload_builder::{RestingPredicateStore, RestingPredicates};

    pub(super) fn new() -> RestingPredicateStore {
        RestingPredicateStore::new(reth_tasks::Runtime::test())
    }

    pub(super) fn finish_and_put(
        store: &RestingPredicateStore,
        resting: RestingPredicates,
        block_hash: B256,
    ) {
        store.put(store.finish(resting), block_hash);
    }
}

/// Untimed pause between finishing a block and starting the next, from
/// `RESTING_SWEEP_BENCH_GAP_MS`, standing in for the state root and leeway an end-of-block
/// sweep overlaps in the builder. Unset, the next block starts immediately.
fn block_gap() -> Option<Duration> {
    std::env::var("RESTING_SWEEP_BENCH_GAP_MS").ok().map(|ms| {
        Duration::from_millis(ms.parse().expect("RESTING_SWEEP_BENCH_GAP_MS is a number"))
    })
}

struct BlockTiming {
    block: u64,
    carry: Duration,
    selection: Duration,
    carried: usize,
}

fn main() {
    let (pool, db) = resting_pool();
    let ctx = payload_context();
    let rejection_cache = RejectionCache::new(100_000, Duration::from_secs(1_800));
    let store = store::new();
    let gap = block_gap();
    let mut churn = 0u64;
    let mut timings = Vec::new();

    for block in 1..=WARMUP_BLOCKS + MEASURED_BLOCKS {
        let start = Instant::now();
        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
        resting.begin_block(block_hash(block - 1), block, Some(&TransitionState::default()));
        let carry = start.elapsed();
        let carried = resting.len();

        let mut best = BestFlashblocksTxs::new(parkable(&pool), rejection_cache.clone())
            .with_resting_predicate_mode(RestingPredicateMode::Enforce)
            .with_resting_predicates(resting);
        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        let executed = run_flashblocks(&ctx, &pool, &mut state, &mut best);
        assert_eq!(executed, FLASHBLOCKS * FILLER_PER_FLASHBLOCK);
        let selection = start.elapsed() - carry;

        let mut resting = best.take_resting_predicates();
        for _ in 0..CHURN_PER_BLOCK {
            resting.rest(keccak256(churn.to_be_bytes()), churn_predicate(churn));
            churn += 1;
        }
        store::finish_and_put(&store, resting, block_hash(block));
        if let Some(gap) = gap {
            std::thread::sleep(gap);
        }

        if block > WARMUP_BLOCKS {
            timings.push(BlockTiming { block, carry, selection, carried });
        }
    }

    println!("gap_ms={}", gap.map_or(0, |gap| gap.as_millis()));
    println!(
        "{:>6} {:>9} {:>12} {:>14} {:>12}",
        "block", "carried", "carry (ms)", "selection (ms)", "total (ms)"
    );
    for t in &timings {
        println!(
            "{:>6} {:>9} {:>12.3} {:>14.3} {:>12.3}",
            t.block,
            t.carried,
            t.carry.as_secs_f64() * 1e3,
            t.selection.as_secs_f64() * 1e3,
            (t.carry + t.selection).as_secs_f64() * 1e3
        );
    }
    let summarize = |label: &str, filter: &dyn Fn(&BlockTiming) -> bool| {
        let mut carry: Vec<f64> =
            timings.iter().filter(|t| filter(t)).map(|t| t.carry.as_secs_f64() * 1e3).collect();
        let mut total: Vec<f64> = timings
            .iter()
            .filter(|t| filter(t))
            .map(|t| (t.carry + t.selection).as_secs_f64() * 1e3)
            .collect();
        carry.sort_by(f64::total_cmp);
        total.sort_by(f64::total_cmp);
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let quantile = |v: &[f64], q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        println!(
            "SUMMARY {label} blocks={} carry_mean_ms={:.3} carry_p50_ms={:.3} carry_p90_ms={:.3} carry_max_ms={:.3} total_mean_ms={:.3} total_p50_ms={:.3} total_p90_ms={:.3} total_max_ms={:.3}",
            carry.len(),
            mean(&carry),
            quantile(&carry, 0.5),
            quantile(&carry, 0.9),
            quantile(&carry, 1.0),
            mean(&total),
            quantile(&total, 0.5),
            quantile(&total, 0.9),
            quantile(&total, 1.0),
        );
    };
    summarize("all", &|_| true);
    summarize("sweep_blocks", &|t| t.block.is_multiple_of(RESTING_UNSEEN_RETAINED_BLOCKS));
    summarize("other_blocks", &|t| !t.block.is_multiple_of(RESTING_UNSEEN_RETAINED_BLOCKS));
}
