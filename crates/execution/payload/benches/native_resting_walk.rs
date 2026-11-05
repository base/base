//! Filler a native (post-Denim) build includes when a resting set larger than one block's walk
//! ranks above it, with a fresh pool iterator per block and with the cut walk carried into the
//! next block.
//!
//! The pool holds `RESTING_WALK_BENCH_SIZES` (default 50000,100000) validity transactions that
//! rest for the whole run and rank above [`FILLER`] plain transfers. Each of [`BLOCKS`] blocks gets
//! [`BUDGET`] before its finalization is requested, which interrupts the walk at the hold's next
//! interrupt check. A fresh iterator walks the resting set from the top every block; a carried
//! one continues where finalization cut the previous block, as the native builder does from Denim
//! on.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_resting_walk
//! ```

use std::{
    mem::ManuallyDrop,
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
    BasePayloadBuilderAttributes, CARRIED_WALK_MIN_HOLDS, CarryingTransactions,
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, ParkableBestPayloadTransactions,
    RestingPredicateMode, RestingPredicateStore, SharedRestingPredicates, WalkWatch,
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
use reth_tasks::CancelOnDrop;
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
/// Plain transfers that fill each block.
const FILLER_PER_BLOCK: usize = 120;
/// Plain transfers in the pool, ranked below every resting transaction.
const FILLER: usize = 2_000;
/// Blocks built back to back.
const BLOCKS: u64 = 10;
/// Selection time each block gets before its finalization is requested.
const BUDGET: Duration = Duration::from_millis(20);
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

/// `resting` never-satisfied validity transactions, each watching its own balance and resting
/// in the returned set, plus [`FILLER`] transfers, and the parent state that funds every sender.
fn resting_pool(resting: usize) -> (Pool, InMemoryDB, Vec<(B256, ValidityPredicate)>) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    let mut members = Vec::with_capacity(resting);
    for sender_index in 0..resting + FILLER {
        let (priority_fee, predicates) = if sender_index < resting {
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
        let transaction = transaction(sender_index, priority_fee, predicates);
        if let Some(predicate) = transaction.transaction.validity_predicates().next() {
            members.push((*transaction.hash(), predicate.clone()));
        }
        pool.add_transaction(transaction, 0);
        db.insert_account_info(
            address(SENDER_OFFSET + sender_index),
            AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
        );
    }
    (pool, db, members)
}

fn payload_context(block: u64) -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    let gas_limit = FILLER_PER_BLOCK as u64 * TRANSFER_GAS;
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        gas_limit,
        number: block - 1,
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

/// Requests `cancel`'s finalization once [`BUDGET`] has passed, as the payload job does when the
/// block's deadline arrives.
fn finalize_after_budget(cancel: &CancelOnDrop) {
    // A dropped clone cancels the job, so the timer's clone is never dropped.
    let cancel = ManuallyDrop::new(cancel.clone());
    std::thread::spawn(move || {
        std::thread::sleep(BUDGET);
        cancel.request_finalization();
    });
}

struct BlockResult {
    filler: usize,
}

/// Builds [`BLOCKS`] blocks over `resting` members, carrying the cut walk when `carry` is set,
/// and returns the filler each block included.
fn run(
    pool: &Pool,
    db: &InMemoryDB,
    members: &[(B256, ValidityPredicate)],
    carry: bool,
) -> Vec<BlockResult> {
    let store = RestingPredicateStore::new(reth_tasks::Runtime::test());
    let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
    resting.begin_block(block_hash(0), 0, Some(&TransitionState::default()));
    for (hash, predicate) in members {
        resting.rest(*hash, predicate.clone());
    }
    store.put(store.finish(resting), block_hash(0));

    let mut carried = None;
    let mut results = Vec::new();
    for block in 1..=BLOCKS {
        let ctx = payload_context(block);
        let cancel_watch = Arc::new(ctx.cancel.clone());
        let mut resting = store.take(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, SWEEP_WAIT_LIMIT);
        resting.begin_block(block_hash(block - 1), block, Some(&TransitionState::default()));

        let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
        let mut builder = ctx.block_builder(&mut state).expect("block builder");
        let mut info = ExecutionInfo::default();

        let (mut walk, shared, watch) = match carried.take() {
            Some((mut walk, shared, watch)) if carry => {
                let released = SharedRestingPredicates::adopt(&shared, resting);
                CarryingTransactions::resume(&mut walk, released);
                (walk, shared, watch)
            }
            _ => {
                let watch = WalkWatch::default();
                let shared =
                    SharedRestingPredicates::new(resting).with_interrupt(watch.interrupt());
                let walk = CarryingTransactions::new(
                    ParkableBestPayloadTransactions::new(Box::new(
                        ParkedBestTransactions::new(pool.best(), Ordering::coinbase_tip(), 0)
                            .with_hold(Some(shared.iterator_hold())),
                    )),
                    Some(shared.clone()),
                );
                (walk, shared, watch)
            }
        };
        watch.watch(&cancel_watch);
        let holds_before = shared.holds();
        let start = Instant::now();
        finalize_after_budget(&ctx.cancel);
        ctx.execute_best_transactions(&mut info, &mut builder, &mut walk, Some(&shared))
            .expect("in-memory execution cannot fail");
        let filler = builder.executor().receipts().len();
        // Waits out the budget so the walk is cut by finalization, as a block with no more pool
        // transactions waits for its deadline.
        std::thread::sleep(BUDGET.saturating_sub(start.elapsed()));
        walk.carry();
        let resting = shared.take_predicates();
        store.put(store.finish(resting), block_hash(block));
        let cut = watch.take_cut() | walk.stopped_after_hold();
        if cut && shared.holds() - holds_before >= CARRIED_WALK_MIN_HOLDS {
            carried = Some((walk, shared, watch));
        }
        drop(cancel_watch);
        results.push(BlockResult { filler });
    }
    results
}

fn main() {
    let sizes: Vec<usize> = std::env::var("RESTING_WALK_BENCH_SIZES")
        .unwrap_or_else(|_| "50000,100000".to_owned())
        .split(',')
        .map(|size| size.parse().expect("RESTING_WALK_BENCH_SIZES is a list of numbers"))
        .collect();
    for resting in sizes {
        let (pool, db, members) = resting_pool(resting);
        for carry in [false, true] {
            let results = run(&pool, &db, &members, carry);
            let mode = if carry { "carried" } else { "refresh" };
            let per_block =
                results.iter().map(|result| result.filler.to_string()).collect::<Vec<_>>();
            let first = results.iter().position(|result| result.filler > 0);
            println!(
                "resting={resting} mode={mode} budget_ms={} first_filler_block={} filler_per_block=[{}] total_filler={}",
                BUDGET.as_millis(),
                first.map_or_else(|| "never".to_owned(), |index| (index + 1).to_string()),
                per_block.join(","),
                results.iter().map(|result| result.filler).sum::<usize>(),
            );
        }
    }
}
