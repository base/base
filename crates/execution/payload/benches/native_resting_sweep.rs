//! Per-block latency of the native (post-Denim) payload builder carrying a large resting set,
//! including the blocks that sweep members no build yielded.
//!
//! The pool holds [`LIVE_RESTING`] validity transactions whose predicates never become satisfied,
//! so every block yields them and holds them back, plus one block of plain transfers. Each block
//! also rests [`CHURN_PER_BLOCK`] members whose transactions then leave the pool, so the sweep
//! always has members to drop and the set stays at a steady size.
//!
//! Blocks run back to back through the same store calls as the native builder: take the set,
//! carry it into the block, run the selection loop, then finish the block and store the set. By
//! default the next block starts immediately, so a sweep still running when it takes the set is
//! counted against that block rather than hidden behind a state root.
//! `RESTING_SWEEP_BENCH_GAP_MS` adds an untimed pause between blocks to model the state root. The
//! table reports the build path of each block from taking the set to the end of the selection
//! loop.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_resting_sweep
//! ```

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
    ParkableBestPayloadTransactions, RESTING_UNSEEN_RETAINED_BLOCKS, RestingPredicateMode,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::db::State;
use reth_transaction_pool::{
    TransactionOrigin, ValidPoolTransaction, identifier::TransactionId, pool::PendingPool,
};
use revm::{
    database::{InMemoryDB, TransitionState},
    state::AccountInfo,
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

const CHAIN_ID: u64 = 8_453;
/// Resting validity transactions the pool yields in every block.
const LIVE_RESTING: usize = 100_000;
/// Members rested in each block whose transactions leave the pool afterwards.
const CHURN_PER_BLOCK: u64 = 4_800;
/// Plain transfers that fill each block.
const FILLER_PER_BLOCK: usize = 120;
/// Blocks built before measuring, so the set reaches its steady-state size.
const WARMUP_BLOCKS: u64 = 3 * RESTING_UNSEEN_RETAINED_BLOCKS;
/// Blocks measured.
const MEASURED_BLOCKS: u64 = 4 * RESTING_UNSEEN_RETAINED_BLOCKS;
/// Long enough that a block always waits for the previous block's sweep, so carry includes it.
const SWEEP_WAIT_LIMIT: Duration = Duration::from_secs(1);
const TRANSFER_GAS: u64 = 21_000;
const SENDER_OFFSET: usize = 1_000_000;
const DENIM_TIMESTAMP: u64 = 1;

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
/// one block of filler, and the parent state that funds every sender.
fn resting_pool() -> (Pool, InMemoryDB) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    for sender_index in 0..LIVE_RESTING + FILLER_PER_BLOCK {
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

fn payload_context() -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    let gas_limit = FILLER_PER_BLOCK as u64 * TRANSFER_GAS;
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
    let store = store::new();
    let gap = block_gap();
    let mut churn = 0u64;
    let mut timings = Vec::new();

    for block in 1..=WARMUP_BLOCKS + MEASURED_BLOCKS {
        let start = Instant::now();
        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
        resting.begin_block(block_hash(block - 1), block, Some(&TransitionState::default()));
        let carry = start.elapsed();
        let carried = resting.len();

        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let mut info = ExecutionInfo::default();
        let best = ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
            pool.best(),
            Ordering::coinbase_tip(),
            0,
        )));
        ctx.execute_best_transactions(&mut info, &mut builder, best, Some(&mut resting))
            .expect("in-memory execution cannot fail");
        assert_eq!(builder.executor().receipts().len(), FILLER_PER_BLOCK);
        let selection = start.elapsed() - carry;

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
