//! Fixture for the `build_loop/mainnet_block` workload: a whole 11-flashblock block shaped like
//! Base mainnet, shared by the wall-clock bench (`build_loop.rs`) and the deterministic
//! instruction-count gate (`build_loop_iai.rs`) so both measure the same workload.

use std::{
    iter,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256, keccak256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
    FlashblockDiagnostics, ParkableBestPayloadTransactions, RejectionCache, ResourceLimits,
    RestingPredicateMode,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
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
use revm::{database::InMemoryDB, state::AccountInfo};

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

/// Wraps a pool snapshot the way `payload.rs` does at the start of each flashblock.
pub(crate) fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    best.no_updates();
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        Ordering::coinbase_tip(),
        0,
    )))
}

/// Flashblocks per 2 s mainnet block.
pub(crate) const MAINNET_FLASHBLOCKS: usize = 11;
/// Plain transfers that arrive, and are included, in each flashblock.
pub(crate) const MAINNET_FILLER_PER_FLASHBLOCK: usize = 17;
/// Of each flashblock's filler, how many send value to a watched address and wake its bucket.
pub(crate) const MAINNET_WAKES_PER_FLASHBLOCK: usize = 5;
/// Candidates over the gas limit that stay in the pool and are rejected in every flashblock.
pub(crate) const MAINNET_OVER_GAS_CANDIDATES: usize = 5;
/// Candidates over the per-transaction DA limit that arrive in each flashblock. Their rejection
/// is permanent, so the builder removes them from the pool afterwards.
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
    ctx: BasePayloadBuilderCtx,
    pre_state: InMemoryDB,
    /// Pending pool at the start of each flashblock: the parked validity transactions and the
    /// over-gas candidates, plus that flashblock's arrivals. Earlier filler and over-DA
    /// candidates are absent because the builder prunes committed and removes permanently
    /// rejected transactions after each flashblock.
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

    pub(crate) fn new() -> Self {
        let mut ctx = builder_context();
        // Evaluating every parked transaction is the work under test. A time budget would also
        // make the outcome counts depend on machine speed.
        ctx.builder_config.predicate_eval_hard_cutoff = Duration::MAX;

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
            .map(|(sender_id, bucket)| Self::parked_tx(sender_id, bucket))
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

        Self { ctx, pre_state, pools, bucket_depths }
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
    /// value.
    pub(crate) fn parked_tx(
        sender_id: usize,
        bucket: usize,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        PoolTx {
            priority_fee: spread_priority_fee(MAINNET_VALIDITY_TIP, sender_id),
            predicates: vec![ValidityPredicate::Balance {
                address: Self::watched(bucket),
                op: ValidityOperator::Equal,
                value: U256::MAX,
            }],
            ..Self::transfer(sender_id)
        }
        .into_pooled()
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

    pub(crate) fn fresh_state(&self) -> State<InMemoryDB> {
        State::builder().with_database(self.pre_state.clone()).with_bundle_update().build()
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
        state: &mut State<InMemoryDB>,
        rejection_cache: &RejectionCache,
        mode: RestingPredicateMode,
    ) -> BlockTotals {
        let mut info = ExecutionInfo::default();
        let mut deferrals = BlockDeferrals::default();
        let mut totals = BlockTotals::default();
        let mut best = BestFlashblocksTxs::new(parkable(&self.pools[0]), rejection_cache.clone())
            .with_resting_predicate_mode(mode);

        for (flashblock, pool) in self.pools.iter().enumerate() {
            if flashblock > 0 {
                best.refresh_iterator(parkable(pool));
            }
            let committed_until = info.executed_transactions.len();
            let diag = self
                .ctx
                .execute_best_transactions(
                    &mut info,
                    &mut deferrals,
                    state,
                    &mut best,
                    &Self::limits(flashblock),
                )
                .expect("build loop succeeds");
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
        totals
    }

    /// Outcomes the workload is designed to produce.
    ///
    /// With resting predicates off, every flashblock defers every parked transaction. Enforced,
    /// only the first flashblock does; later ones defer just the transactions a wake promotes.
    /// Buckets at or above the ordered threshold wake only transactions whose `Equal` value is
    /// reached, so a wake promotes none of theirs. The resting index uses the same default
    /// threshold as the builder config.
    pub(crate) fn expected_totals(&self, mode: RestingPredicateMode) -> BlockTotals {
        let ordered_threshold = self.ctx.builder_config.predicate_bucket_ordered_threshold;
        let deferred = match mode {
            RestingPredicateMode::Off => MAINNET_PARKED * MAINNET_FLASHBLOCKS,
            RestingPredicateMode::Enforce => {
                let rewoken: usize = (MAINNET_WAKES_PER_FLASHBLOCK
                    ..MAINNET_FLASHBLOCKS * MAINNET_WAKES_PER_FLASHBLOCK)
                    .map(|wake| self.bucket_depths[Self::woken_bucket(wake)])
                    .filter(|&depth| depth < ordered_threshold)
                    .sum();
                MAINNET_PARKED + rewoken
            }
            RestingPredicateMode::Shadow => {
                unimplemented!("the mainnet block measures Off and Enforce")
            }
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
