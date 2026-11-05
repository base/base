//! Per-flashblock cost of walking the pool iterator past a large set of resting validity
//! transactions, split by layer.
//!
//! The pool holds `resting` validity transactions from distinct senders whose predicates stay
//! unsatisfied and that outrank [`ORDINARY`] plain transfers, the shape of a resting flood. Each
//! row is the median of [`ROUNDS`] fresh iterators over the same pool, which is what the
//! flashblocks builder creates at every flashblock:
//!
//! - `construct`: `PendingPool::best`, the reth iterator snapshot.
//! - `reth`: draining the reth iterator alone.
//! - `park`: draining it through `ParkedBestTransactions` with a hold that parks every validity
//!   transaction without a lookup, the lane and parking bookkeeping alone.
//! - `hold`: the resting hold, which also looks up every candidate in the resting set.
//! - `drop`: dropping the drained parking iterator, which `refresh_iterator` does inside the
//!   builder's pool-fetch timing.
//! - `first_ordinary`: from a fresh flashblock iterator to the first ordinary transaction through
//!   `BestFlashblocksTxs`, the latency the build loop sees.
//!
//! A second table runs one block of [`FLASHBLOCKS`] flashblocks whose cancel fires after
//! `RESTING_WALK_BENCH_WINDOW_MS` (default 20, roughly the walk time a 170 ms production
//! flashblock leaves on a machine about three times faster), once refreshing the iterator every
//! flashblock, as before resuming existed, and once through `BestFlashblocksTxs::begin_flashblock`,
//! which resumes a walk the cancel cut. It reports the first flashblock that
//! reaches an ordinary transaction and how many ordinary transactions the block includes.
//!
//! ```sh
//! cargo bench -p base-builder-core --bench resting_walk
//! ```

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use base_builder_core::{
    BestFlashblocksTxs, FlashblockDiagnostics, ParkableBestPayloadTransactions,
    ParkablePayloadTransactions, RejectionCache, RestingPayloadTransactions, RestingPredicateMode,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, RestingPredicates, SharedRestingPredicates,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, BestTransactionsHold, ParkedBestTransactions,
    ValidityOperator, ValidityPredicate,
};
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use tokio_util::sync::CancellationToken;

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;

const CHAIN_ID: u64 = 901;
/// Plain transfers ranked below every resting transaction.
const ORDINARY: usize = 2_000;
/// Fresh iterators measured per row.
const ROUNDS: usize = 15;
/// Resting set sizes measured.
const RESTING_SIZES: [usize; 3] = [10_000, 50_000, 100_000];
/// Flashblocks per simulated block.
const FLASHBLOCKS: usize = 10;

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
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(Address::repeat_byte(0xee)),
        value: U256::from(sender_index),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), address(sender_index)),
        encoded_length,
    )
    .with_validity_predicates(predicates);
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new((sender_index as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// A pool with `resting` validity transactions above [`ORDINARY`] plain ones, and the resting
/// set holding every validity transaction.
fn resting_pool(resting: usize) -> (Pool, SharedRestingPredicates) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut set = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
    for sender_index in 0..resting + ORDINARY {
        let transaction = if sender_index < resting {
            let predicate = ValidityPredicate::Balance {
                address: address(sender_index),
                op: ValidityOperator::Equal,
                value: U256::ONE,
            };
            let transaction = transaction(sender_index, 10, vec![predicate.clone()]);
            set.rest(*transaction.hash(), predicate);
            transaction
        } else {
            transaction(sender_index, 1, Vec::new())
        };
        pool.add_transaction(transaction, 0);
    }
    (pool, SharedRestingPredicates::new(set))
}

/// Parks every validity candidate without a lookup.
struct ParkAll;

impl BestTransactionsHold<BasePooledTransaction> for ParkAll {
    fn hold(&mut self, _transaction: &ValidPoolTransaction<BasePooledTransaction>) -> bool {
        true
    }

    fn record_duration(&mut self, _duration: Duration) {}
}

fn reth_best(
    pool: &Pool,
) -> impl BestTransactions<Item = Arc<ValidPoolTransaction<BasePooledTransaction>>> + use<> {
    let mut best = pool.best();
    best.no_updates();
    best
}

/// Drains a parking iterator with `hold` and returns the drain and drop times.
fn walk(
    pool: &Pool,
    hold: Box<dyn BestTransactionsHold<BasePooledTransaction>>,
) -> (Duration, Duration) {
    let mut parked = ParkedBestTransactions::new(reth_best(pool), Ordering::coinbase_tip(), 0)
        .with_hold(Some(hold));
    let start = Instant::now();
    let yielded = parked.by_ref().count();
    let drain = start.elapsed();
    assert_eq!(yielded, ORDINARY);
    let start = Instant::now();
    drop(parked);
    (drain, start.elapsed())
}

type BestTxs = BestFlashblocksTxs<
    BasePooledTransaction,
    ParkableBestPayloadTransactions<BasePooledTransaction>,
>;

fn parkable(
    pool: &Pool,
    hold: Option<Box<dyn BestTransactionsHold<BasePooledTransaction>>>,
) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    ParkableBestPayloadTransactions::new(Box::new(
        ParkedBestTransactions::new(reth_best(pool), Ordering::coinbase_tip(), 0).with_hold(hold),
    ))
}

/// Runs one block whose flashblocks are each cancelled after `window`, and returns the first
/// flashblock (1-based) that yielded an ordinary transaction and the ordinary transactions
/// included, playing the build loop: include every yielded transaction until the cancel.
fn simulate_block(
    pool: &Pool,
    shared: &SharedRestingPredicates,
    window: Duration,
    resume: bool,
) -> (Option<usize>, usize) {
    let rejection_cache = RejectionCache::new(100_000, Duration::from_secs(1_800));
    let mut best: BestTxs = BestFlashblocksTxs::new(parkable(pool, None), rejection_cache)
        .with_resting_predicate_mode(RestingPredicateMode::Enforce)
        .with_resting_predicates(shared.clone());
    let mut first = None;
    let mut included = 0;
    for flashblock in 1..=FLASHBLOCKS {
        let cancel = CancellationToken::new();
        let timer = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(window);
                cancel.cancel();
            })
        };
        if resume {
            best.begin_flashblock(cancel.clone(), |hold| parkable(pool, hold));
        } else {
            best.set_cancel(cancel.clone());
            let hold = best.iterator_hold();
            best.refresh_iterator(parkable(pool, hold));
        }
        let mut diagnostics = FlashblockDiagnostics::default();
        while best.next(()).is_some() {
            if cancel.is_cancelled() {
                diagnostics.cancelled = true;
                break;
            }
            best.mark_current_committed();
            included += 1;
            first.get_or_insert(flashblock);
        }
        diagnostics.cancelled |= cancel.is_cancelled();
        timer.join().expect("timer thread");
        let _ = best.take_resting_stats();
        best.finish_flashblock(&diagnostics);
    }
    (first, included)
}

fn median(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1e3
}

fn main() {
    println!(
        "{:>8} {:>10} {:>8} {:>8} {:>8} {:>8} {:>15}",
        "resting", "construct", "reth", "park", "hold", "drop", "first_ordinary"
    );
    for resting in RESTING_SIZES {
        let (pool, shared) = resting_pool(resting);
        let rejection_cache = RejectionCache::new(100_000, Duration::from_secs(1_800));
        let mut samples: [Vec<Duration>; 6] = Default::default();
        for _ in 0..ROUNDS {
            let start = Instant::now();
            let best = reth_best(&pool);
            samples[0].push(start.elapsed());

            let start = Instant::now();
            assert_eq!(best.count(), resting + ORDINARY);
            samples[1].push(start.elapsed());

            let (park, drop_time) = walk(&pool, Box::new(ParkAll));
            samples[2].push(park);
            samples[4].push(drop_time);
            samples[3].push(walk(&pool, shared.iterator_hold()).0);

            let mut best = BestFlashblocksTxs::new(
                ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
                    reth_best(&pool),
                    Ordering::coinbase_tip(),
                    0,
                ))),
                rejection_cache.clone(),
            )
            .with_resting_predicate_mode(RestingPredicateMode::Enforce)
            .with_resting_predicates(shared.clone());
            let start = Instant::now();
            let hold = best.iterator_hold();
            best.refresh_iterator(ParkableBestPayloadTransactions::new(Box::new(
                ParkedBestTransactions::new(reth_best(&pool), Ordering::coinbase_tip(), 0)
                    .with_hold(hold),
            )));
            let first = best.next(()).expect("an ordinary transaction is reachable");
            samples[5].push(start.elapsed());
            assert!(first.transaction.validity_conditions().is_empty());
        }
        let [construct, reth, park, hold, drop_time, first] = samples.map(median);
        println!(
            "{resting:>8} {construct:>10.3} {reth:>8.3} {park:>8.3} {hold:>8.3} {drop_time:>8.3} {first:>15.3}"
        );
    }

    let window = Duration::from_millis(
        std::env::var("RESTING_WALK_BENCH_WINDOW_MS")
            .ok()
            .map_or(20, |ms| ms.parse().expect("RESTING_WALK_BENCH_WINDOW_MS is a number")),
    );
    println!();
    println!("block of {FLASHBLOCKS} flashblocks, each cancelled after {} ms", window.as_millis());
    println!(
        "{:>8} {:>18} {:>17} {:>17} {:>16}",
        "resting", "refresh_first_fb", "refresh_included", "resume_first_fb", "resume_included"
    );
    for resting in RESTING_SIZES {
        let (pool, shared) = resting_pool(resting);
        let (refresh_first, refresh_included) = simulate_block(&pool, &shared, window, false);
        let (resume_first, resume_included) = simulate_block(&pool, &shared, window, true);
        let show =
            |first: Option<usize>| first.map_or_else(|| "never".to_string(), |f| f.to_string());
        println!(
            "{resting:>8} {:>18} {refresh_included:>17} {:>17} {resume_included:>16}",
            show(refresh_first),
            show(resume_first)
        );
    }
}
