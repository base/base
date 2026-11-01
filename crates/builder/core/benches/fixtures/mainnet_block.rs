//! Fixture for the `build_loop/mainnet_block` workload: a whole 11-flashblock block shaped like
//! Base mainnet, shared by the wall-clock bench (`build_loop.rs`) and the deterministic
//! instruction-count gate (`build_loop_iai.rs`) so both measure the same workload.
//!
//! [`BenchKnobs`] adds what an in-memory database hides: a delay on every state read that misses
//! the build's cache, standing in for a cold database read on the sequencer, and the
//! validity-predicate evaluation budget the builder enforces. Both default off, which keeps the
//! instruction-count gate deterministic.

use std::{
    iter,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
    FlashblockDiagnostics, ParkableBestPayloadTransactions, RejectionCache, ResourceLimits,
    RestingPredicateMode,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::{RestingPredicates, SharedRestingPredicates};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, BasePooledTx as _, BestTransactionsHold, IngestionRest,
    ParkedBestTransactions, ValidityOperator, ValidityPredicate,
};
use reth_chainspec::ChainSpec;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::State;
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use revm::{Database, bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

pub(crate) type Ordering = BaseOrdering<BasePooledTransaction>;
pub(crate) type Pool = PendingPool<Ordering>;
pub(crate) const CHAIN_ID: u64 = 901;
pub(crate) const PARENT_GAS_LIMIT: u64 = 30_000_000;
pub(crate) const PARENT_BASE_FEE: u64 = 1_000_000_000;
pub(crate) const MAX_FEE_PER_GAS: u128 = 10_000_000_000;
pub(crate) const TX_DATA_LIMIT: u64 = 1_000;
pub(crate) const OVERSIZED_CALLDATA_BYTES: usize = 4_096;
pub(crate) const SENDER_BALANCE: u128 = 10u128.pow(24);
pub(crate) const REJECTION_CACHE_TTL: Duration = Duration::from_secs(60);

/// Deterministic address in a numbered namespace.
pub(crate) fn address(namespace: u64, index: usize) -> Address {
    Address::from_word(keccak256([namespace.to_be_bytes(), (index as u64).to_be_bytes()].concat()))
}

/// Deterministic calldata that compresses poorly, so its estimated DA size tracks its length.
pub(crate) fn incompressible_calldata(seed: usize, len: usize) -> Bytes {
    (0..len.div_ceil(32))
        .flat_map(|chunk| keccak256([seed.to_be_bytes(), chunk.to_be_bytes()].concat()).0)
        .take(len)
        .collect()
}

/// A benchmark transaction before it is signed and wrapped for the pool.
pub(crate) struct PoolTx {
    /// Pool sender id; every benchmark transaction has its own sender at nonce zero.
    pub(crate) sender_id: usize,
    pub(crate) sender: Address,
    pub(crate) to: Address,
    pub(crate) gas_limit: u64,
    pub(crate) priority_fee: u128,
    pub(crate) value: U256,
    pub(crate) input: Bytes,
    pub(crate) predicates: Vec<ValidityPredicate>,
}

impl PoolTx {
    pub(crate) fn into_pooled(self) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit: self.gas_limit,
            max_fee_per_gas: MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: self.priority_fee,
            to: TxKind::Call(self.to),
            value: self.value,
            access_list: Default::default(),
            input: self.input,
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), self.sender),
            encoded_length,
        )
        .with_validity_predicates(self.predicates);

        // Pool validation computes the DA size before a transaction reaches the pool, so the
        // builder reads the cached value. Prime it here to match.
        transaction.estimated_compressed_size();

        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new((self.sender_id as u64).into(), 0),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }
}

/// Priority fee that spreads candidates over a thousand tip levels above `base`.
pub(crate) const fn spread_priority_fee(base: u128, index: usize) -> u128 {
    base + 1_000_000 * (1 + (index as u128 * 7_919) % 1_000)
}

/// Builder context for the next block on top of a parent with production-like gas and base fee.
pub(crate) fn builder_context() -> BasePayloadBuilderCtx {
    let genesis = serde_json::from_value(serde_json::json!({
        "config": { "chainId": CHAIN_ID },
        "gasLimit": format!("{PARENT_GAS_LIMIT:#x}"),
        "timestamp": "0x0"
    }))
    .expect("valid genesis");
    let chain_spec = Arc::new(BaseChainSpec::from(
        ChainSpec::builder().chain(CHAIN_ID.into()).genesis(genesis).cancun_activated().build(),
    ));
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        gas_limit: PARENT_GAS_LIMIT,
        base_fee_per_gas: Some(PARENT_BASE_FEE),
        ..Default::default()
    }));
    BasePayloadBuilderCtx::for_test(chain_spec, parent)
}

/// Wraps a pool snapshot the way `payload.rs` does at the start of each flashblock, with the
/// hold an enforcing build hands the pool iterator.
pub(crate) fn parkable(
    pool: &Pool,
    hold: Option<Box<dyn BestTransactionsHold<BasePooledTransaction>>>,
) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    best.no_updates();
    ParkableBestPayloadTransactions::new(Box::new(
        ParkedBestTransactions::new(best, Ordering::coinbase_tip(), 0).with_hold(hold),
    ))
}

/// Flashblocks per 2 s mainnet block.
pub(crate) const MAINNET_FLASHBLOCKS: usize = 11;
/// Plain transfers that arrive, and are included, in each flashblock.
pub(crate) const MAINNET_FILLER_PER_FLASHBLOCK: usize = 17;
/// Of each flashblock's filler, how many send value to a watched address and wake its bucket.
pub(crate) const MAINNET_WAKES_PER_FLASHBLOCK: usize = 5;
/// Candidates over the gas limit that stay in the pool and are rejected in every flashblock.
pub(crate) const MAINNET_OVER_GAS_CANDIDATES: usize = 5;
/// Candidates over the per-transaction DA limit that arrive in each flashblock and are rejected
/// in it.
pub(crate) const MAINNET_OVER_DA_PER_FLASHBLOCK: usize = 5;
/// Parked validity transactions present in every flashblock build.
pub(crate) const MAINNET_PARKED: usize = 2_840;
/// Distinct watched addresses read by the parked transactions' predicates.
pub(crate) const MAINNET_PREDICATE_BUCKETS: usize = 518;
/// `(bucket count, depth)` tiers of the parked transactions: median depth 2, p99 depth 46.
pub(crate) const MAINNET_BUCKET_TIERS: [(usize, usize); 8] =
    [(160, 1), (100, 2), (40, 3), (118, 6), (32, 7), (50, 16), (10, 26), (8, 46)];
/// Coprime with [`MAINNET_PREDICATE_BUCKETS`], so successive wakes hit distinct buckets spread
/// over every depth tier.
pub(crate) const MAINNET_WAKE_STRIDE: usize = 47;
/// Calldata that brings a mainnet transfer to the 459-byte median mainnet transaction size.
pub(crate) const MAINNET_CALLDATA_BYTES: usize = 344;
/// Covers the intrinsic gas of a transfer carrying [`MAINNET_CALLDATA_BYTES`] of calldata.
pub(crate) const MAINNET_TX_GAS_LIMIT: u64 = 30_000;
/// Tip floor for validity transactions, above every other candidate's tip, so each flashblock
/// considers them first.
pub(crate) const MAINNET_VALIDITY_TIP: u128 = 2_000_000_000;

/// What the in-memory workload adds to stand in for production conditions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BenchKnobs {
    /// Added to every account or storage read that misses the build's cache, standing in for a
    /// cold database read. Repeated reads hit the cache, as on the sequencer.
    pub(crate) read_delay: Duration,
    /// The builder's `predicate_eval_hard_cutoff`, or no budget. With a budget, which candidates
    /// are evaluated depends on machine speed.
    pub(crate) predicate_budget: Option<Duration>,
}

/// [`InMemoryDB`] whose account and storage reads take at least `delay`.
///
/// [`State`] caches what it loads, so the delay applies once per location per block, like a cold
/// read that later reads find in the cache. The delay spins rather than sleeps, because sleeps
/// shorter than the scheduler tick overshoot.
#[derive(Debug, Clone)]
pub(crate) struct DelayedDb {
    inner: InMemoryDB,
    delay: Duration,
}

impl DelayedDb {
    fn wait(&self) {
        if self.delay.is_zero() {
            return;
        }
        let start = Instant::now();
        while start.elapsed() < self.delay {
            std::hint::spin_loop();
        }
    }
}

impl Database for DelayedDb {
    type Error = <InMemoryDB as Database>::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.wait();
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.wait();
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

/// How the block treats validity transactions whose predicate is unsatisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Workload {
    /// Resting predicates off: every flashblock evaluates every parked transaction.
    Off,
    /// Resting predicates enforced: the first flashblock evaluates every parked transaction, and
    /// the pool iterator holds them back in later ones.
    Enforce,
    /// Resting predicates enforced, and the pool iterator adopts the pool's admission-time
    /// evaluation: no flashblock evaluates a parked transaction, and the build loop never sees
    /// one until a wake promotes it.
    EnforceIngestion,
}

impl Workload {
    pub(crate) const ALL: [Self; 3] = [Self::Off, Self::Enforce, Self::EnforceIngestion];

    pub(crate) const fn resting_mode(self) -> RestingPredicateMode {
        match self {
            Self::Off => RestingPredicateMode::Off,
            Self::Enforce | Self::EnforceIngestion => RestingPredicateMode::Enforce,
        }
    }

    pub(crate) const fn ingestion_mode(self) -> RestingPredicateMode {
        match self {
            Self::Off | Self::Enforce => RestingPredicateMode::Off,
            Self::EnforceIngestion => RestingPredicateMode::Enforce,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Enforce => "Enforce",
            Self::EnforceIngestion => "Enforce+ingestion",
        }
    }
}

/// Timings of one block build, for the summary the wall-clock bench prints.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BlockProfile {
    /// Wall time of each flashblock's build loop.
    pub(crate) flashblock_times: Vec<Duration>,
    /// Flashblocks built until every parked transaction rested, or `None` if some never did.
    pub(crate) flashblocks_to_index: Option<usize>,
}

/// Outcomes summed over every flashblock of a block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockTotals {
    pub(crate) considered: u64,
    pub(crate) included: u64,
    pub(crate) deferred: u64,
    pub(crate) rejected_gas: u64,
    pub(crate) rejected_da: u64,
    pub(crate) rejected_other: u64,
}

impl BlockTotals {
    pub(crate) const fn add(&mut self, diag: &FlashblockDiagnostics) {
        self.considered += diag.txs_considered;
        self.included += diag.txs_included;
        self.deferred += diag.txs_deferred;
        self.rejected_gas += diag.txs_rejected_gas;
        self.rejected_da += diag.txs_rejected_da;
        self.rejected_other += diag.txs_rejected_other;
    }
}

/// A whole block shaped like Base mainnet.
///
/// Every flashblock first considers the parked validity transactions, whose balance predicates
/// never match, then includes its filler transfers and rejects its over-limit candidates. A few
/// filler transfers send value to watched addresses, waking their predicate buckets so the
/// parked transactions in them are evaluated again and stay unsatisfied.
pub(crate) struct MainnetBlock {
    /// One builder context per [`Workload`], in [`Workload::ALL`] order.
    ctxs: Vec<BasePayloadBuilderCtx>,
    pre_state: InMemoryDB,
    knobs: BenchKnobs,
    /// Pending pool at the start of each flashblock: the parked validity transactions and the
    /// over-gas candidates, plus that flashblock's arrivals. Earlier filler is absent because
    /// the builder prunes committed transactions after each flashblock. Earlier over-DA
    /// candidates are left out so each flashblock rejects only its own arrivals.
    pools: Vec<Pool>,
    /// Number of parked transactions watching each bucket's address.
    bucket_depths: Vec<usize>,
}

impl MainnetBlock {
    pub(crate) const FILLER_START: usize = MAINNET_PARKED;
    pub(crate) const OVER_GAS_START: usize =
        Self::FILLER_START + MAINNET_FLASHBLOCKS * MAINNET_FILLER_PER_FLASHBLOCK;
    pub(crate) const OVER_DA_START: usize = Self::OVER_GAS_START + MAINNET_OVER_GAS_CANDIDATES;
    pub(crate) const SENDER_COUNT: usize =
        Self::OVER_DA_START + MAINNET_FLASHBLOCKS * MAINNET_OVER_DA_PER_FLASHBLOCK;

    pub(crate) fn with_knobs(knobs: BenchKnobs) -> Self {
        let ctxs: Vec<_> = Workload::ALL
            .into_iter()
            .map(|workload| {
                let mut ctx = builder_context();
                // Without a budget, evaluating every parked transaction is the work under test,
                // and the outcome counts do not depend on machine speed.
                ctx.builder_config.predicate_eval_hard_cutoff =
                    knobs.predicate_budget.unwrap_or(Duration::MAX);
                ctx.builder_config.resting_predicate_mode = workload.resting_mode();
                ctx.builder_config.ingestion_rest_mode = workload.ingestion_mode();
                ctx
            })
            .collect();
        let parent_hash = ctxs[0].parent().hash();

        let bucket_depths: Vec<usize> = MAINNET_BUCKET_TIERS
            .iter()
            .flat_map(|&(count, depth)| iter::repeat_n(depth, count))
            .collect();
        assert_eq!(
            bucket_depths.len(),
            MAINNET_PREDICATE_BUCKETS,
            "bucket tiers cover every bucket"
        );
        assert_eq!(
            bucket_depths.iter().sum::<usize>(),
            MAINNET_PARKED,
            "bucket tiers hold every parked transaction"
        );

        let parked: Vec<_> = bucket_depths
            .iter()
            .enumerate()
            .flat_map(|(bucket, &depth)| iter::repeat_n(bucket, depth))
            .enumerate()
            .map(|(sender_id, bucket)| Self::parked_tx(sender_id, bucket, parent_hash))
            .collect();
        let over_gas: Vec<_> = (Self::OVER_GAS_START..Self::OVER_DA_START)
            .map(|sender_id| {
                PoolTx { gas_limit: PARENT_GAS_LIMIT, ..Self::transfer(sender_id) }.into_pooled()
            })
            .collect();

        let pools = (0..MAINNET_FLASHBLOCKS)
            .map(|flashblock| {
                let filler = (0..MAINNET_FILLER_PER_FLASHBLOCK)
                    .map(|slot| Self::filler_tx(flashblock * MAINNET_FILLER_PER_FLASHBLOCK + slot));
                let over_da = (0..MAINNET_OVER_DA_PER_FLASHBLOCK).map(|slot| {
                    Self::over_da_tx(
                        Self::OVER_DA_START + flashblock * MAINNET_OVER_DA_PER_FLASHBLOCK + slot,
                    )
                });
                let mut pool = PendingPool::new(Ordering::coinbase_tip());
                for tx in parked.iter().chain(&over_gas).cloned().chain(filler).chain(over_da) {
                    pool.add_transaction(tx, 0);
                }
                pool
            })
            .collect();

        let mut pre_state = InMemoryDB::default();
        for sender_id in 0..Self::SENDER_COUNT {
            pre_state.insert_account_info(
                Self::sender(sender_id),
                AccountInfo::from_balance(U256::from(SENDER_BALANCE)),
            );
        }

        Self { ctxs, pre_state, knobs, pools, bucket_depths }
    }

    fn ctx(&self, workload: Workload) -> &BasePayloadBuilderCtx {
        &self.ctxs[workload as usize]
    }

    pub(crate) fn sender(sender_id: usize) -> Address {
        address(5, sender_id)
    }

    pub(crate) fn watched(bucket: usize) -> Address {
        address(6, bucket)
    }

    /// Bucket whose watched address the `wake`-th wake of the block sends value to.
    pub(crate) const fn woken_bucket(wake: usize) -> usize {
        wake * MAINNET_WAKE_STRIDE % MAINNET_PREDICATE_BUCKETS
    }

    /// A median-sized transfer to a fresh account, tipped below every validity transaction.
    pub(crate) fn transfer(sender_id: usize) -> PoolTx {
        PoolTx {
            sender_id,
            sender: Self::sender(sender_id),
            to: address(7, sender_id),
            gas_limit: MAINNET_TX_GAS_LIMIT,
            priority_fee: spread_priority_fee(0, sender_id),
            value: U256::from(1 + sender_id),
            input: incompressible_calldata(sender_id, MAINNET_CALLDATA_BYTES),
            predicates: Vec::new(),
        }
    }

    /// A validity transaction waiting for its bucket's watched balance to reach an unreachable
    /// value. It carries the pool's admission-time evaluation on `parent_hash`, as the pool
    /// records it with `--rollup.mempool-validity-ingestion-rest`; workloads without ingestion
    /// rest ignore it.
    pub(crate) fn parked_tx(
        sender_id: usize,
        bucket: usize,
        parent_hash: B256,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let predicate = ValidityPredicate::Balance {
            address: Self::watched(bucket),
            op: ValidityOperator::Equal,
            value: U256::MAX,
        };
        let pooled = PoolTx {
            priority_fee: spread_priority_fee(MAINNET_VALIDITY_TIP, sender_id),
            predicates: vec![predicate.clone()],
            ..Self::transfer(sender_id)
        }
        .into_pooled();
        pooled
            .transaction
            .set_ingestion_rest(IngestionRest { state_block_hash: parent_hash, predicate });
        pooled
    }

    /// The `filler`-th filler transfer of the block. The first few of each flashblock wake a
    /// bucket.
    pub(crate) fn filler_tx(filler: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let sender_id = Self::FILLER_START + filler;
        let flashblock = filler / MAINNET_FILLER_PER_FLASHBLOCK;
        let slot = filler % MAINNET_FILLER_PER_FLASHBLOCK;
        let mut tx = Self::transfer(sender_id);
        if slot < MAINNET_WAKES_PER_FLASHBLOCK {
            let wake = flashblock * MAINNET_WAKES_PER_FLASHBLOCK + slot;
            tx.to = Self::watched(Self::woken_bucket(wake));
        }
        tx.into_pooled()
    }

    pub(crate) fn over_da_tx(sender_id: usize) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        PoolTx {
            gas_limit: 200_000,
            input: incompressible_calldata(sender_id, OVERSIZED_CALLDATA_BYTES),
            ..Self::transfer(sender_id)
        }
        .into_pooled()
    }

    /// Cumulative limits that admit exactly the filler that has arrived by `flashblock`.
    pub(crate) fn limits(flashblock: usize) -> ResourceLimits {
        ResourceLimits {
            block_gas_limit: ((flashblock + 1) * MAINNET_FILLER_PER_FLASHBLOCK) as u64
                * MAINNET_TX_GAS_LIMIT,
            tx_data_limit: Some(TX_DATA_LIMIT),
            ..Default::default()
        }
    }

    pub(crate) fn fresh_state(&self) -> State<DelayedDb> {
        let db = DelayedDb { inner: self.pre_state.clone(), delay: self.knobs.read_delay };
        State::builder().with_database(db).with_bundle_update().build()
    }

    /// An empty rejection cache. Created outside the timed region, as the builder's cache
    /// outlives every block.
    pub(crate) fn rejection_cache(&self) -> RejectionCache {
        RejectionCache::new(Self::SENDER_COUNT as u64, REJECTION_CACHE_TTL)
    }

    /// Builds every flashblock of the block, driving the candidate iterator as `payload.rs`
    /// does between flashblocks.
    ///
    /// `rejection_cache` stands in for the builder's long-lived cache, which each payload job
    /// clones.
    pub(crate) fn build(
        &self,
        state: &mut State<DelayedDb>,
        rejection_cache: &RejectionCache,
        workload: Workload,
    ) -> BlockTotals {
        self.build_profiled(state, rejection_cache, workload).0
    }

    /// [`Self::build`], also returning the per-flashblock timings.
    pub(crate) fn build_profiled(
        &self,
        state: &mut State<DelayedDb>,
        rejection_cache: &RejectionCache,
        workload: Workload,
    ) -> (BlockTotals, BlockProfile) {
        let ctx = self.ctx(workload);
        let mut info = ExecutionInfo::default();
        let mut deferrals = BlockDeferrals::default();
        let mut totals = BlockTotals::default();
        let mut profile = BlockProfile::default();
        // As `payload.rs` sets up a block: a resting set that starts empty, so every parked
        // transaction is seen for the first time, tracking written keys when the admission
        // results are consulted, with the pre-block writes known. Every flashblock's pool
        // iterator, the first included, takes the hold of the same set.
        let mut resting =
            RestingPredicates::new(ctx.builder_config.predicate_bucket_ordered_threshold);
        resting.track_written_keys(workload.ingestion_mode().is_enabled());
        resting.begin_block(ctx.parent().hash(), ctx.block_number(), Some(&Default::default()));
        let resting = SharedRestingPredicates::new(resting)
            .with_ingestion_rest_adoption(workload.ingestion_mode().is_enforced());
        let hold = || workload.resting_mode().is_enforced().then(|| resting.iterator_hold());
        let mut best =
            BestFlashblocksTxs::new(parkable(&self.pools[0], hold()), rejection_cache.clone())
                .with_resting_predicate_mode(workload.resting_mode())
                .with_resting_predicates(resting.clone());

        for (flashblock, pool) in self.pools.iter().enumerate() {
            if flashblock > 0 {
                best.refresh_iterator(parkable(pool, hold()));
            }
            let committed_until = info.executed_transactions.len();
            let start = Instant::now();
            let diag = ctx
                .execute_best_transactions(
                    &mut info,
                    &mut deferrals,
                    state,
                    &mut best,
                    &Self::limits(flashblock),
                )
                .expect("build loop succeeds");
            profile.flashblock_times.push(start.elapsed());
            if profile.flashblocks_to_index.is_none() && resting.len() >= MAINNET_PARKED {
                profile.flashblocks_to_index = Some(flashblock + 1);
            }
            let committed: Vec<_> = info.executed_transactions[committed_until..]
                .iter()
                .map(|tx| tx.tx_hash())
                .collect();
            best.mark_committed(&committed);
            if !diag.permanently_rejected_txs.is_empty() {
                best.mark_rejected(&diag.permanently_rejected_txs);
            }
            totals.add(&diag);
        }
        (totals, profile)
    }

    /// Outcomes the workload is designed to produce, with no predicate budget.
    ///
    /// With resting predicates off, every flashblock defers every parked transaction. Enforced,
    /// only the first flashblock does; later ones defer just the transactions a wake promotes.
    /// Adopting the admission results, no flashblock defers a parked transaction, because the
    /// pool iterator parks them before the build loop sees them, and the build loop defers only
    /// those a wake promotes, starting with the first flashblock's wakes. Buckets at or above the
    /// ordered threshold wake only transactions whose `Equal` value is reached, so a wake promotes
    /// none of theirs. The resting index uses the same default threshold as the builder config.
    pub(crate) fn expected_totals(&self, workload: Workload) -> BlockTotals {
        let ordered_threshold =
            self.ctx(workload).builder_config.predicate_bucket_ordered_threshold;
        let rewoken = |first_wake: usize| -> usize {
            (first_wake..MAINNET_FLASHBLOCKS * MAINNET_WAKES_PER_FLASHBLOCK)
                .map(|wake| self.bucket_depths[Self::woken_bucket(wake)])
                .filter(|&depth| depth < ordered_threshold)
                .sum()
        };
        let deferred = match workload {
            Workload::Off => MAINNET_PARKED * MAINNET_FLASHBLOCKS,
            Workload::Enforce => MAINNET_PARKED + rewoken(MAINNET_WAKES_PER_FLASHBLOCK),
            Workload::EnforceIngestion => rewoken(0),
        } as u64;
        let included = (MAINNET_FLASHBLOCKS * MAINNET_FILLER_PER_FLASHBLOCK) as u64;
        let rejected_gas = (MAINNET_FLASHBLOCKS * MAINNET_OVER_GAS_CANDIDATES) as u64;
        let rejected_da = (MAINNET_FLASHBLOCKS * MAINNET_OVER_DA_PER_FLASHBLOCK) as u64;
        BlockTotals {
            considered: deferred + included + rejected_gas + rejected_da,
            included,
            deferred,
            rejected_gas,
            rejected_da,
            rejected_other: 0,
        }
    }
}
