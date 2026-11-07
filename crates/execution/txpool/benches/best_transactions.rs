//! Per-build cost of `best_transactions` on a large `BaseTransactionPool`.
//!
//! The native builder creates one merged best-transactions iterator per block build. Each arm
//! measures that path with reth's pending and queued subpools and the validity pool filled to the
//! order of their configured limits:
//!
//! - `snapshot`: creating the iterator, which includes the cross-store lane bookkeeping the
//!   merge needs (validity lanes riding a standard run, standard transactions queued behind a
//!   validity run).
//! - `first_300`: the snapshot plus yielding a block's worth of candidates.
//! - `drain_shared`: the snapshot plus yielding every shared validity head and the standard
//!   transactions it releases, so the merge accumulates many released lanes at once.
//!
//! `dormant_scaling` admits 1 000 ready validity transactions and a growing number of others,
//! all ready at first, then classification parks all but the 1 000 with dormancy enforced; its
//! snapshot time should stay flat as the dormant count grows. In `dormant_scaling_shared` every
//! sender also has a standard transaction queued behind its validity one, so the snapshot bridges
//! the 1 000 ready lanes and must skip the dormant ones; `dormant_scaling_nonce_free` holds
//! nonce-free EIP-8130 transactions instead.
//!
//! `dormancy_commit_wake` measures what each commit of a build owning the dormancy index pays to
//! find the transactions it may wake, when it wakes none, as the dormant count grows.
//!
//! `dormant` is the `isolated` pool with dormancy enforced and every validity transaction
//! blocked on an unset storage slot, so the snapshot walks only the standard transactions.
//! `dormant_shared` makes 20 000 of those dormant validity transactions shared lanes with a
//! standard successor queued behind each, so bridging must skip them without visiting them.
//!
//! The `isolated` pool has no sender with transactions in two stores. `shared` adds 1 000
//! senders whose standard transaction is queued right behind their validity transaction, which
//! is the case the merge has to bridge; there the standard successor outbids everything else, so
//! it is taken right after its validity head. `shared_waiting` prices those successors below
//! everything else instead, so every released successor waits in the merge while the rest of
//! the pool is selected.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench best_transactions
//! ```

mod support;

use std::hint::black_box;

use alloy_consensus::transaction::Recovered;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, Eip8130Constants, Eip8130Signed, TxEip8130,
};
use base_execution_txpool::{
    BasePooledTransaction, ParkableTransactionPool, StateDiffInvalidation, ValidityDormancyMode,
    ValidityOperator, ValidityPredicate,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_transaction_pool::{
    BestTransactionsAttributes, PoolTransaction, TransactionOrigin, TransactionPool,
};
use revm::state::{Account, EvmState, EvmStorageSlot};
use support::{BenchPool, build_pool, funded_signer, standard, validity};

/// Standard transactions pending in reth, one per sender.
const PENDING_STANDARD: usize = 10_000;
/// Standard transactions queued in reth behind a nonce gap; also the queued subpool limit.
const QUEUED_STANDARD: usize = 20_000;
/// Resting validity transactions, one per sender.
const VALIDITY: usize = 25_000;
/// Senders holding a validity transaction with a standard one queued behind it.
const SHARED_LANES: usize = 1_000;
/// Shared lanes in `dormant_shared`: every queued standard transaction rides a dormant root.
const DORMANT_SHARED_LANES: usize = QUEUED_STANDARD;
/// Candidates a block's worth of selection pulls from the iterator.
const BLOCK_CANDIDATES: usize = 300;
/// Tip of the standard successor in `shared`: above every other transaction in the pool.
const OUTBIDDING_TIP: u128 = 60;
/// Tip of the standard successor in `shared_waiting`: below every other transaction.
const WAITING_TIP: u128 = 1;

async fn add(pool: &BenchPool, transactions: Vec<BasePooledTransaction>) {
    for transaction in transactions {
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
    }
}

/// A validity transaction blocked until a storage slot nobody writes holds one.
fn dormant_validity(signer: &PrivateKeySigner, tip: u128) -> BasePooledTransaction {
    let mut predicates =
        validity(signer, 0, tip).validity_conditions().submitted().cloned().collect::<Vec<_>>();
    predicates.push(ValidityPredicate::Storage {
        address: Address::repeat_byte(0xcc),
        slot: U256::ZERO,
        mask: U256::MAX,
        op: ValidityOperator::Equal,
        value: U256::ONE,
    });
    standard(signer, 0, tip).with_validity_predicates(predicates)
}

/// A pool filled to the sizes above. `shared_lanes` senders hold both a resting validity
/// transaction at nonce 0 and a standard transaction at nonce 1 paying `successor_tip`, which
/// reth queues behind the gap.
///
/// The runtime hosting the validation tasks is returned with the pool so it outlives the
/// measurement.
fn large_pool(
    shared_lanes: usize,
    successor_tip: u128,
    dormancy: ValidityDormancyMode,
) -> (BenchPool, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pool = runtime.block_on(async {
        let (pool, client) = build_pool(QUEUED_STANDARD);
        let pool = pool.with_validity_dormancy(dormancy);
        let resting_validity = |signer: &PrivateKeySigner, tip| match dormancy {
            ValidityDormancyMode::Off => validity(signer, 0, tip),
            ValidityDormancyMode::Enforce => dormant_validity(signer, tip),
        };
        let pending: Vec<_> = (0..PENDING_STANDARD)
            .map(|index| standard(&funded_signer(&client), 0, 2 + index as u128 % 50))
            .collect();
        add(&pool, pending).await;
        let queued: Vec<_> = (0..QUEUED_STANDARD - shared_lanes)
            .map(|index| standard(&funded_signer(&client), 1, 2 + index as u128 % 50))
            .collect();
        add(&pool, queued).await;
        let resting: Vec<_> = (0..VALIDITY - shared_lanes)
            .map(|index| resting_validity(&funded_signer(&client), 2 + index as u128 % 50))
            .collect();
        add(&pool, resting).await;
        let mut shared = Vec::with_capacity(shared_lanes * 2);
        for index in 0..shared_lanes {
            let signer = funded_signer(&client);
            shared.push(resting_validity(&signer, 2 + index as u128 % 50));
            shared.push(standard(&signer, 1, successor_tip));
        }
        add(&pool, shared).await;
        classify(&pool);
        pool
    });
    // A dormant root ends its lane's pending run, so dormant validity transactions are queued.
    let dormant = if dormancy.is_enforced() { VALIDITY } else { 0 };
    let size = pool.pool_size();
    assert_eq!(size.pending, PENDING_STANDARD + VALIDITY - dormant, "ready validity is pending");
    assert_eq!(size.queued, QUEUED_STANDARD + dormant, "standard successors are queued");
    (pool, runtime)
}

/// The builder seals block 1, which the canonical chain then reaches, so queued transactions are
/// classified.
fn classify(pool: &BenchPool) {
    let block_hash = B256::repeat_byte(1);
    if let Some(build) = pool.begin_dormancy_build(B256::ZERO, 1, None) {
        build.seal(block_hash);
    }
    pool.advance_dormancy(&[BlockNumHash::new(1, block_hash)]);
}

/// Ready validity transactions in every `dormant_scaling` pool.
const SCALING_READY: usize = 1_000;

/// The validity transactions a `dormant_scaling` pool holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalingKind {
    /// Lane roots.
    Lanes,
    /// Lane roots, each with a standard transaction queued behind.
    SharedLanes,
    /// Nonce-free EIP-8130 transactions.
    NonceFree,
}

/// The block-number bound every `dormant_scaling` transaction carries, so it rests.
const fn resting_bound() -> ValidityPredicate {
    ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from_limbs([1_000, 0, 0, 0]),
    }
}

/// A nonce-free EIP-8130 transaction from `signer` paying `fee` per gas, blocked until a storage
/// slot nobody writes holds one when `blocked`.
fn nonce_free(signer: &PrivateKeySigner, fee: u128, blocked: bool) -> BasePooledTransaction {
    let tx = TxEip8130 {
        chain_id: ChainConfig::mainnet().chain_id,
        sender: None,
        nonce_key: Eip8130Constants::NONCE_KEY_MAX,
        nonce_sequence: 0,
        valid_after: 0,
        valid_before: NONCE_FREE_VALID_BEFORE_MS,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: fee,
        gas_limit: 50_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    };
    let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
    let signed = Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
    let mut predicates = vec![resting_bound()];
    if blocked {
        predicates.push(ValidityPredicate::Storage {
            address: Address::repeat_byte(0xcc),
            slot: U256::ZERO,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::ONE,
        });
    }
    BasePooledTransaction::from_pooled(Recovered::new_unchecked(
        ConsensusPooledTransaction::Eip8130(signed),
        signer.address(),
    ))
    .with_validity_predicates(predicates)
}

/// Nonce-free expiry a few seconds past the bench pool's clock, in milliseconds.
const NONCE_FREE_VALID_BEFORE_MS: u64 = 1_700_000_005_000;

/// A pool enforcing dormancy with [`SCALING_READY`] ready validity transactions and `dormant`
/// dormant ones of `kind`, each from its own sender and all admitted ready before classification
/// parks the dormant ones.
fn dormant_scaling_pool(dormant: usize, kind: ScalingKind) -> (BenchPool, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pool = runtime.block_on(async {
        let (pool, client) = build_pool(SCALING_READY + DORMANT_SCALING.last().unwrap());
        let pool = pool.with_validity_dormancy(ValidityDormancyMode::Enforce);
        let mut transactions = Vec::new();
        for index in 0..SCALING_READY + dormant {
            let signer = funded_signer(&client);
            let tip = 2 + index as u128 % 50;
            let ready = index < SCALING_READY;
            transactions.push(match (kind, ready) {
                (ScalingKind::NonceFree, _) => nonce_free(&signer, 1_000 + tip, !ready),
                (_, true) => validity(&signer, 0, tip),
                (_, false) => dormant_validity(&signer, tip),
            });
            if kind == ScalingKind::SharedLanes {
                transactions.push(standard(&signer, 1, tip));
            }
        }
        add(&pool, transactions).await;
        classify(&pool);
        pool
    });
    // A dormant nonce-free transaction still counts as pending.
    let pending =
        if kind == ScalingKind::NonceFree { SCALING_READY + dormant } else { SCALING_READY };
    assert_eq!(pool.pool_size().pending, pending, "dormant lane roots are queued");
    let offered = if kind == ScalingKind::SharedLanes { 2 * SCALING_READY } else { SCALING_READY };
    assert_eq!(pool.best_transactions().count(), offered, "ready lanes and their bridged runs");
    (pool, runtime)
}

/// Dormant transaction counts the `dormant_scaling` arms hold.
const DORMANT_SCALING: [usize; 3] = [0, 25_000, 75_000];

fn dormant_scaling_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("txpool/best_transactions");
    group.sample_size(20);
    for (name, kind) in [
        ("dormant_scaling", ScalingKind::Lanes),
        ("dormant_scaling_shared", ScalingKind::SharedLanes),
        ("dormant_scaling_nonce_free", ScalingKind::NonceFree),
    ] {
        for dormant in DORMANT_SCALING {
            let (pool, _runtime) = dormant_scaling_pool(dormant, kind);
            let id = BenchmarkId::new(format!("{name}/snapshot"), dormant);
            group.bench_with_input(id, &pool, |b, pool| {
                b.iter(|| black_box(pool.best_transactions()));
            });
        }
    }
    group.finish();
}

fn best_transactions_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("txpool/best_transactions");
    group.sample_size(20);
    for (name, shared_lanes, successor_tip, dormancy) in [
        ("isolated", 0, OUTBIDDING_TIP, ValidityDormancyMode::Off),
        ("dormant", 0, OUTBIDDING_TIP, ValidityDormancyMode::Enforce),
        ("dormant_shared", DORMANT_SHARED_LANES, OUTBIDDING_TIP, ValidityDormancyMode::Enforce),
        ("shared", SHARED_LANES, OUTBIDDING_TIP, ValidityDormancyMode::Off),
        ("shared_waiting", SHARED_LANES, WAITING_TIP, ValidityDormancyMode::Off),
    ] {
        let (pool, _runtime) = large_pool(shared_lanes, successor_tip, dormancy);
        group.bench_with_input(BenchmarkId::new("snapshot", name), &pool, |b, pool| {
            b.iter(|| black_box(pool.best_transactions()));
        });
        group.bench_with_input(BenchmarkId::new("first_300", name), &pool, |b, pool| {
            b.iter(|| {
                let best = pool.best_transactions();
                assert_eq!(best.take(BLOCK_CANDIDATES).map(black_box).count(), BLOCK_CANDIDATES);
            });
        });
        if shared_lanes == 0 || dormancy == ValidityDormancyMode::Enforce {
            continue;
        }
        // Every pending transaction plus the shared successors; the whole pool
        // is drained, so each released successor is yielded wherever its tip
        // ranks it.
        let drained = PENDING_STANDARD + VALIDITY + shared_lanes;
        group.bench_with_input(BenchmarkId::new("drain_shared", name), &pool, |b, pool| {
            b.iter(|| {
                let best = pool.best_transactions();
                assert_eq!(best.map(black_box).count(), drained);
            });
        });
    }
    group.finish();
}

/// One committed transaction's state: a storage write no dormant transaction is blocked on.
fn unrelated_commit() -> EvmState {
    let mut account = Account::default();
    account.storage.insert(
        U256::from(7),
        EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), Default::default()),
    );
    account.mark_touch();
    EvmState::from_iter([(Address::repeat_byte(0xdd), account)])
}

/// What each commit of a build owning the dormancy index costs it to look up the transactions
/// it may wake, in the common case where it wakes none, with a growing number of dormant ones.
fn dormancy_commit_wake_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("txpool/dormancy_commit_wake");
    let commit = unrelated_commit();
    for dormant in DORMANT_SCALING {
        let (pool, _runtime) = dormant_scaling_pool(dormant, ScalingKind::Lanes);
        let build = pool.begin_dormancy_build(B256::repeat_byte(1), 2, None).unwrap();
        let mut best = pool.best_transactions_with_attributes_and_parking(
            BestTransactionsAttributes::new(0, None),
            None,
            Some(&build),
        );
        group.bench_function(BenchmarkId::new("commit", dormant), |b| {
            b.iter(|| best.wake_by_state(black_box(&commit)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    best_transactions_benches,
    dormant_scaling_benches,
    dormancy_commit_wake_benches
);
criterion_main!(benches);
