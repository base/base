//! Flashblocks adapters for parkable best-transaction iterators.

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use alloy_primitives::{Address, TxHash, map::B256Set};
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, ParkablePayloadTransactions, RestingPredicates,
    SharedRestingPredicates,
};
use base_execution_txpool::{
    BasePooledTx, BestTransactionsHold, ValidityConditions, ValidityPredicate,
};
use parking_lot::Mutex;
use reth_payload_util::PayloadTransactions;
use revm::state::EvmState;
use tokio_util::sync::CancellationToken;

use crate::{
    BuilderMetrics, CounterHandle, FlashblockDiagnostics, RejectionCache,
    RestingPayloadTransactions, RestingPredicateMode, RestingStats,
};

/// Resting transactions a flashblock must have held before the next flashblock resumes its cut
/// walk.
///
/// A flashblock that held fewer had little resting walk to save, for example because the walk
/// already passed the resting set, so the next flashblock refreshes and sees the current pool.
pub const RESTING_WALK_RESUME_MIN_HOLDS: u64 = 64;

/// Why a resting walk cut by a flashblock cancel was not continued by the next flashblock.
///
/// Each cut walk is either resumed, counted by `resting_walk_resumed_total`, or reset for one of
/// these reasons, counted by `resting_walk_reset_total`, so the two add up to the cut walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestingWalkReset {
    /// The walk was cut in the last flashblock of a block and would have been resumed, but the
    /// next block starts a fresh iterator.
    BlockEnd,
    /// The cut flashblock skipped transactions for a limit a later flashblock may satisfy.
    LimitRejection,
    /// The cut flashblock held fewer than [`RESTING_WALK_RESUME_MIN_HOLDS`] resting transactions.
    FewHolds,
}

impl RestingWalkReset {
    /// Returns the metric label value of this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockEnd => "block_end",
            Self::LimitRejection => "limit_rejection",
            Self::FewHolds => "few_holds",
        }
    }

    /// Counts one cut walk reset for this reason.
    pub fn record(self) {
        BuilderMetrics::resting_walk_reset_total(self.as_str()).increment(1);
    }
}

/// An adapter that skips transactions already committed or permanently rejected by flashblocks.
///
/// It also owns the validity transactions resting under an unchanged predicate, see
/// [`RestingPayloadTransactions`]. Under [`RestingPredicateMode::Enforce`], every pool iterator
/// the flashblocks builder creates takes an [`Self::iterator_hold`] that parks resting members at
/// the source, so their nonce lane stays blocked without the build loop seeing them. A commit that
/// changes the state a member's predicate reads promotes it back at its priority position. The
/// hold also stops the pool iterator once the flashblock is cancelled.
///
/// A flashblock cancelled while its iterator walks a long run of held transactions leaves that
/// walk cut. [`Self::begin_flashblock`] lets the next flashblock of the block continue the same
/// iterator instead of building a fresh one and re-walking the run from the top, so a resting set
/// that takes longer than one flashblock to walk is walked once rather than never finished.
pub struct BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    inner: I,
    // Transactions that were already committed to the state. Using them again would cause NonceTooLow
    // so we skip them
    committed_transactions: B256Set,
    // Shared cross-block rejection cache (survives across blocks, TTL-bounded)
    rejection_cache: RejectionCache,
    // Identity of the transaction most recently returned to the build loop.
    current_transaction: Option<(TxHash, Address, u64)>,
    resting_predicate_mode: RestingPredicateMode,
    // Transactions resting under the predicate last found unsatisfied, carried across flashblocks
    // and blocks, and shared with the holds of the pool iterators.
    resting: SharedRestingPredicates,
    // Resolved on the first hit so later hits on the candidate path skip the registry lookup.
    rejection_cache_hits: Option<CounterHandle>,
    // Cancel token of the flashblock being built, read by the current iterator's hold, which
    // outlives a flashblock when a cut walk resumes.
    walk_cancel: Arc<Mutex<CancellationToken>>,
    // Set by the hold's interrupt, or by `finish_flashblock`, when a cancel cut the current
    // flashblock's walk. Cleared by `finish_flashblock`, which turns it into `carry_pending`.
    walk_cut: Arc<AtomicBool>,
    // Whether the next flashblock of this block continues the current iterator.
    carry_pending: bool,
    // Resting transactions held during the last flashblock whose stats were taken.
    flashblock_holds: u64,
    // Transactions the build loop parked in the current iterator, re-offered when a cut walk
    // resumes as a fresh iterator would. Tracked only under enforce, the only mode that resumes.
    parked_by_build: B256Set,
    // `SharedRestingPredicates::hold_runs` when the build loop last received a candidate.
    seen_hold_runs: u64,
    // Whether the iterator held resting transactions since the previous candidate, which covers
    // walking to the current one and, when it came from the ready set, pre-filling the next.
    current_after_hold: bool,
    transaction: PhantomData<T>,
}

/// Returns a hold that parks members of `resting` in a flashblock's pool iterator and stops the
/// iterator's walk past them once `cancel`, the flashblock's cancellation, fires. The build loop
/// checks the same token before each candidate, so a deadline that fires mid-walk ends the
/// flashblock as promptly as one that fires between candidates.
pub fn flashblock_hold<T: BasePooledTx>(
    resting: &SharedRestingPredicates,
    cancel: &CancellationToken,
) -> Box<dyn BestTransactionsHold<T>> {
    let cancel = cancel.clone();
    resting.clone().with_interrupt(Arc::new(move || cancel.is_cancelled())).iterator_hold()
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("committed_transactions", &self.committed_transactions)
            .field("rejection_cache_size", &self.rejection_cache.entry_count())
            .field("resting_predicate_mode", &self.resting_predicate_mode)
            .field("resting", &self.resting.len())
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    /// Creates a new [`BestFlashblocksTxs`] wrapping the given payload transaction iterator.
    pub fn new(inner: I, rejection_cache: RejectionCache) -> Self {
        Self {
            inner,
            committed_transactions: Default::default(),
            rejection_cache,
            current_transaction: None,
            resting_predicate_mode: RestingPredicateMode::Off,
            resting: SharedRestingPredicates::new(RestingPredicates::new(
                DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
            )),
            rejection_cache_hits: None,
            walk_cancel: Arc::new(Mutex::new(CancellationToken::new())),
            walk_cut: Arc::new(AtomicBool::new(false)),
            carry_pending: false,
            flashblock_holds: 0,
            parked_by_build: Default::default(),
            seen_hold_runs: 0,
            current_after_hold: false,
            transaction: PhantomData,
        }
    }

    /// Sets whether resting validity transactions are tracked and held back, within a block and
    /// across blocks.
    #[must_use]
    pub const fn with_resting_predicate_mode(mut self, mode: RestingPredicateMode) -> Self {
        self.resting_predicate_mode = mode;
        self
    }

    /// Starts from `resting`, typically carried over from the previous block. The pool iterators
    /// this adapter wraps must take their hold from [`Self::iterator_hold`] of the same adapter.
    #[must_use]
    pub fn with_resting_predicates(mut self, resting: SharedRestingPredicates) -> Self {
        self.resting = resting;
        self
    }

    /// Returns the hold a new inner iterator parks resting members with, or `None` when resting
    /// transactions are not enforced and the build loop evaluates them. The hold stops the
    /// iterator's walk once the flashblock passed to [`Self::begin_flashblock`] is cancelled. It
    /// reads the current flashblock's token rather than the one it was created under, because a
    /// resumed iterator keeps its hold into later flashblocks.
    pub fn iterator_hold(&self) -> Option<Box<dyn BestTransactionsHold<T>>> {
        self.resting_predicate_mode.is_enforced().then(|| {
            let cancel = Arc::clone(&self.walk_cancel);
            let cut = Arc::clone(&self.walk_cut);
            self.resting
                .clone()
                .with_interrupt(Arc::new(move || {
                    let interrupted = cancel.lock().is_cancelled();
                    if interrupted {
                        cut.store(true, AtomicOrdering::Relaxed);
                    }
                    interrupted
                }))
                .iterator_hold()
        })
    }

    /// Takes the resting set, leaving an empty one, so it can be carried into the next block.
    pub fn take_resting_predicates(&mut self) -> RestingPredicates {
        self.resting.take_predicates()
    }

    /// Starts a flashblock cancelled by `cancel`. Continues the current iterator if the previous
    /// flashblock's walk can be resumed, see [`Self::resume_cut_walk`], and otherwise replaces it
    /// with `fresh`, given the hold from [`Self::iterator_hold`]. Returns whether it resumed.
    pub fn begin_flashblock(
        &mut self,
        cancel: CancellationToken,
        fresh: impl FnOnce(Option<Box<dyn BestTransactionsHold<T>>>) -> I,
    ) -> bool {
        self.set_cancel(cancel);
        if self.resume_cut_walk() {
            return true;
        }
        let hold = self.iterator_hold();
        self.refresh_iterator(fresh(hold));
        false
    }

    /// Ends a flashblock whose selection produced `diagnostics`, after the build loop took its
    /// resting stats, and decides whether the next flashblock continues the current iterator.
    ///
    /// The walk was cut if the hold's interrupt stopped it, or if a cancelled flashblock's build
    /// loop stopped on a candidate it never decided on, reached right after held transactions,
    /// which covers a cancel after the hold's last periodic check. A flashblock cancelled while
    /// it builds past the resting transactions leaves no cut walk, so the next one refreshes.
    ///
    /// A cut walk is carried into the next flashblock only if this flashblock held at least
    /// [`RESTING_WALK_RESUME_MIN_HOLDS`] resting transactions and skipped nothing for a limit a
    /// later flashblock may satisfy, such as its gas or DA target: the build loop invalidated
    /// those lanes for the rest of the iterator, and only a fresh iterator offers them again.
    /// Otherwise the reset is counted with its [`RestingWalkReset`] reason.
    pub fn finish_flashblock(&mut self, diagnostics: &FlashblockDiagnostics) {
        let interrupted = self.walk_cut.swap(false, AtomicOrdering::Relaxed);
        let stopped_after_hold =
            diagnostics.cancelled && self.current_transaction.is_some() && self.current_after_hold;
        self.carry_pending = false;
        if !self.resting_predicate_mode.is_enforced() || !(interrupted || stopped_after_hold) {
            return;
        }
        if diagnostics.has_retryable_limit_rejections() {
            RestingWalkReset::LimitRejection.record();
        } else if self.flashblock_holds < RESTING_WALK_RESUME_MIN_HOLDS {
            RestingWalkReset::FewHolds.record();
        } else {
            self.carry_pending = true;
        }
    }

    /// Ends the block. A walk the last flashblock left to carry is reset, because the next block
    /// starts a fresh iterator over its own pool snapshot.
    pub fn finish_block(&mut self) {
        if std::mem::take(&mut self.carry_pending) {
            RestingWalkReset::BlockEnd.record();
        }
    }

    /// Sets the cancel token of the flashblock being built, which the current iterator's hold
    /// also checks.
    pub fn set_cancel(&mut self, cancel: CancellationToken) {
        *self.walk_cancel.lock() = cancel;
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries. The new iterator starts with no cut walk to resume.
    pub fn refresh_iterator(&mut self, inner: I) {
        self.inner = inner;
        self.current_transaction = None;
        self.walk_cut.store(false, AtomicOrdering::Relaxed);
        self.carry_pending = false;
        self.flashblock_holds = 0;
        self.parked_by_build.clear();
        self.seen_hold_runs = self.resting.hold_runs();
    }

    /// Continues the current iterator in a new flashblock if [`Self::finish_flashblock`] left the
    /// previous flashblock's cut walk to carry, and returns whether it did. Call
    /// [`Self::set_cancel`] with the new flashblock's token first.
    ///
    /// The resumed iterator keeps every resting transaction it held and its lane state, so it
    /// neither re-walks nor yields them, and a later commit still promotes them. Resting
    /// transactions the build loop parked in this block stay parked the same way. Other
    /// transactions the build loop parked, and a candidate the cancelled flashblock yielded but
    /// never decided on, are promoted back at their priority position, as a fresh iterator would
    /// offer them again.
    ///
    /// The iterator keeps the pool snapshot of the flashblock that started the walk: transactions
    /// that arrive since and rank above the walk's position are not offered until a flashblock
    /// refreshes, and lanes the build loop invalidated for state-dependent reasons, such as an
    /// unaffordable tip, stay invalid for the rest of the walk.
    pub fn resume_cut_walk(&mut self) -> bool {
        if !std::mem::take(&mut self.carry_pending) {
            return false;
        }
        if let Some((transaction_hash, _, _)) = self.current_transaction.take() {
            self.inner.park_current();
            self.inner.promote(transaction_hash);
        }
        // The build loop's predicate index is per flashblock, so the hold takes over waking the
        // resting transactions the build loop parked.
        self.resting.hold_rested_members(&mut self.parked_by_build);
        for transaction_hash in self.parked_by_build.drain() {
            self.inner.promote(transaction_hash);
        }
        BuilderMetrics::resting_walk_resumed_total().increment(1);
        true
    }

    /// Remove transaction from next iteration since it is already in the state
    pub fn mark_committed(&mut self, txs: &[TxHash]) {
        self.committed_transactions.extend(txs);
    }

    /// Mark transactions as permanently rejected. They will be skipped in all
    /// subsequent flashblocks within this block and across future blocks via
    /// the shared rejection cache.
    pub fn mark_rejected(&mut self, tx_hashes: &[TxHash]) {
        self.rejection_cache.mark_rejected(tx_hashes);
        BuilderMetrics::rejection_cache_insertions().increment(tx_hashes.len() as u64);
        BuilderMetrics::rejection_cache_size().set(self.rejection_cache.entry_count() as f64);
    }
}

impl<T, I> PayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    type Transaction = I::Transaction;

    /// Inlined into the build loop, which calls it once per candidate.
    #[inline]
    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let pooled = self.inner.next(ctx)?;
            let tx = &pooled.transaction;
            let hash = *tx.hash();
            self.current_transaction = Some((hash, tx.sender(), tx.nonce()));

            if self.committed_transactions.contains(&hash) {
                self.inner.mark_current_committed();
                self.current_transaction = None;
                continue;
            }

            if self.rejection_cache.is_rejected(&hash) {
                self.rejection_cache_hits
                    .get_or_insert_with(BuilderMetrics::rejection_cache_hits)
                    .increment(1);
                // Only intrinsically invalid transactions enter this cache. Their nonce-lane
                // descendants cannot execute across the resulting gap, so exclude the lane for
                // this iterator rather than treating the rejected head as committed.
                self.inner.mark_invalid(tx.sender(), tx.nonce());
                self.current_transaction = None;
                continue;
            }

            if self.resting_predicate_mode.is_enforced() {
                let hold_runs = self.resting.hold_runs();
                self.current_after_hold = hold_runs != self.seen_hold_runs;
                self.seen_hold_runs = hold_runs;
            }
            return Some(pooled);
        }
    }

    /// Proxy to inner iterator
    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        let matches_current =
            self.current_transaction.is_some_and(|(_, current_sender, current_nonce)| {
                current_sender == sender && current_nonce == nonce
            });
        debug_assert!(matches_current, "mark_invalid must identify the current transaction");
        if !matches_current {
            return;
        }
        self.inner.mark_invalid(sender, nonce);
        self.current_transaction = None;
    }
}

impl<T, I> ParkablePayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    type Pooled = T;

    fn park_current(&mut self) {
        self.inner.park_current();
        if let Some((transaction_hash, _, _)) = self.current_transaction.take()
            && self.resting_predicate_mode.is_enforced()
        {
            self.parked_by_build.insert(transaction_hash);
        }
    }

    fn mark_current_committed(&mut self) {
        self.inner.mark_current_committed();
        if let Some((transaction_hash, _, _)) = self.current_transaction.take() {
            self.committed_transactions.insert(transaction_hash);
        }
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(transaction_hash)
    }
}

impl<T, I> RestingPayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Pooled = T>,
{
    fn rest(&mut self, transaction_hash: TxHash, predicate: &ValidityPredicate) {
        if self.resting_predicate_mode.is_enabled() {
            self.resting.rest(transaction_hash, predicate.clone());
        }
    }

    // With resting off the set stays empty, so these skip its lock rather than take it once per
    // evaluated validity candidate and once per commit.
    fn release(&mut self, transaction_hash: TxHash) {
        if self.resting_predicate_mode.is_enabled() {
            self.resting.remove(transaction_hash);
        }
    }

    fn record_committed_state(&mut self, state: &EvmState) {
        if !self.resting_predicate_mode.is_enabled() {
            return;
        }
        for transaction_hash in self.resting.wake_by_state(state) {
            self.inner.promote(transaction_hash);
        }
    }

    fn is_resting(&self, transaction_hash: TxHash, predicates: &ValidityConditions) -> bool {
        self.resting_predicate_mode.is_enabled()
            && self.resting.member(transaction_hash, predicates).is_some()
    }

    fn take_resting_stats(&mut self) -> RestingStats {
        let stats = self.resting.take_stats();
        // The build loop takes the stats once per flashblock.
        self.flashblock_holds = stats.parked;
        stats
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_consensus::{Header, SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Signature, TxHash, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_txpool::{
        BaseOrdering, BasePooledTransaction, HOLD_INTERRUPT_INTERVAL, ParkedBestTransactions,
        ValidityOperator, ValidityPredicate,
    };
    use reth_chainspec::ChainSpec;
    use reth_payload_util::PayloadTransactions;
    use reth_primitives_traits::{Recovered, SealedHeader};
    use reth_provider::{StateProvider, noop::NoopProvider};
    use reth_revm::{State, database::StateProviderDatabase};
    use reth_transaction_pool::{
        TransactionOrigin, ValidPoolTransaction, identifier::TransactionId, pool::PendingPool,
    };
    use revm::{
        database::TransitionState,
        state::{Account, EvmState},
    };

    use super::*;
    use crate::{
        BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ExecutionInfo,
        ParkableBestPayloadTransactions, ParkablePayloadTransactions, RejectionCache,
        ResourceLimits, RestingPayloadTransactions, RestingPredicateMode, RestingStats,
    };

    type Ordering = BaseOrdering<BasePooledTransaction>;
    type Parkable = ParkableBestPayloadTransactions<BasePooledTransaction>;

    const WATCHED: Address = Address::repeat_byte(0xaa);
    const UNRELATED: Address = Address::repeat_byte(0xbb);

    fn test_rejection_cache() -> RejectionCache {
        RejectionCache::new(1000, Duration::from_secs(60))
    }

    fn sender_address(sender: u64) -> Address {
        Address::from_word(U256::from(sender + 1).into())
    }

    fn transaction(
        sender: u64,
        nonce: u64,
        priority_fee: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        validity_transaction(sender, nonce, priority_fee, Vec::new())
    }

    fn validity_transaction(
        sender: u64,
        nonce: u64,
        priority_fee: u128,
        predicates: Vec<ValidityPredicate>,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: priority_fee + 100,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(Address::ZERO),
            // Distinguishes otherwise identical transactions from different senders, which share
            // the test signature.
            value: U256::from(sender),
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender_address(sender)),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(sender.into(), nonce),
            transaction,
            propagate: true,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    fn pending_pool(
        transactions: &[Arc<ValidPoolTransaction<BasePooledTransaction>>],
    ) -> PendingPool<Ordering> {
        let mut pool = PendingPool::new(Ordering::coinbase_tip());
        for transaction in transactions {
            pool.add_transaction(Arc::clone(transaction), 0);
        }
        pool
    }

    /// Builds the production lane-parking iterator over a snapshot of `pool`.
    fn parkable(
        pool: &PendingPool<Ordering>,
    ) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
        ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
            pool.best(),
            Ordering::coinbase_tip(),
            0,
        )))
    }

    /// Drains `iterator`, marking each yielded transaction invalid for this scan only, and
    /// returns the yielded hashes.
    fn drain_without_including<I>(iterator: &mut I) -> Vec<TxHash>
    where
        I: PayloadTransactions<Transaction = Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    {
        std::iter::from_fn(|| {
            let transaction = iterator.next(())?;
            iterator.mark_invalid(transaction.sender(), transaction.nonce());
            Some(*transaction.hash())
        })
        .collect()
    }

    #[test]
    fn yields_the_pool_transaction_handle() {
        let pooled = transaction(0, 0, 1);
        let pool = pending_pool(&[Arc::clone(&pooled)]);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        let yielded = iterator.next(()).unwrap();

        assert!(Arc::ptr_eq(&yielded, &pooled));
    }

    #[test]
    fn test_simple_case() {
        let pool =
            pending_pool(&[transaction(0, 0, 1), transaction(1, 0, 1), transaction(2, 0, 1)]);

        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());
        // ### First flashblock
        iterator.refresh_iterator(parkable(&pool));
        // Accept first tx
        let tx1 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Invalidate second tx
        let tx2 = iterator.next(()).unwrap();
        iterator.mark_invalid(tx2.sender(), tx2.nonce());
        // Accept third tx
        let tx3 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx1.hash(), *tx3.hash()]);

        // ### Second flashblock
        // It should not return txs 1 and 3, but should return 2
        iterator.refresh_iterator(parkable(&pool));
        let tx2 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx2.hash()]);

        // ### Third flashblock
        iterator.refresh_iterator(parkable(&pool));
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
    }

    #[test]
    fn hashes_marked_committed_are_skipped_after_refresh() {
        let committed = transaction(0, 0, 2);
        let uncommitted = transaction(1, 0, 1);
        let committed_hash = *committed.hash();
        let uncommitted_hash = *uncommitted.hash();
        let pool = pending_pool(&[committed, uncommitted]);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        iterator.mark_committed(&[committed_hash]);
        iterator.refresh_iterator(parkable(&pool));

        assert_eq!(drain_without_including(&mut iterator), vec![uncommitted_hash]);
    }

    /// Rejected transactions are skipped across flashblock boundaries within the same block.
    #[test]
    fn test_rejected_txs_persist_across_refresh() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2, transaction(2, 0, 1)]);

        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        // FB1: none of the transactions are included, and the second is rejected permanently
        assert_eq!(drain_without_including(&mut iterator).len(), 3);
        iterator.mark_rejected(&[tx_2_hash]);

        // FB2: refresh iterator — tx2 should still be skipped
        iterator.refresh_iterator(parkable(&pool));
        let seen_hashes = drain_without_including(&mut iterator);
        assert!(!seen_hashes.contains(&tx_2_hash), "rejected tx should not reappear after refresh");
        assert_eq!(seen_hashes.len(), 2, "only non-rejected txs should appear");
    }

    /// Rejected transactions in the shared cache are skipped by a new iterator instance
    /// (simulating cross-block persistence).
    #[test]
    fn test_rejection_cache_persists_across_blocks() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2]);

        let cache = test_rejection_cache();

        // Block 1: reject tx_2
        let mut iter1 = BestFlashblocksTxs::new(parkable(&pool), cache.clone());
        assert_eq!(drain_without_including(&mut iter1).len(), 2);
        iter1.mark_rejected(&[tx_2_hash]);

        // Block 2: new iterator, same cache — tx_2 should be skipped
        let mut iter2 = BestFlashblocksTxs::new(parkable(&pool), cache);
        let seen_hashes = drain_without_including(&mut iter2);
        assert!(
            !seen_hashes.contains(&tx_2_hash),
            "tx rejected in block 1 should be skipped in block 2"
        );
        assert_eq!(seen_hashes.len(), 1, "only non-rejected tx should appear");
    }

    #[test]
    fn rejection_cache_hit_excludes_nonce_descendants() {
        let rejected = transaction(0, 0, 3);
        let descendant = transaction(0, 1, 2);
        let other = transaction(1, 0, 1);
        let rejected_hash = *rejected.hash();
        let other_hash = *other.hash();
        let pool = pending_pool(&[rejected, descendant, other]);
        let cache = test_rejection_cache();
        cache.insert(rejected_hash);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), cache);

        let yielded_hashes = drain_without_including(&mut iterator);

        assert_eq!(yielded_hashes, vec![other_hash]);
    }

    /// This test simulates the nonce-chain gating fix across flashblock boundaries.
    ///
    /// Scenario (based on real Base Mainnet block 41628995):
    /// - Sender A has `TX_A` (nonce 0, LOW tip) and `TX_B` (nonce 1, HIGH tip) in the pool
    /// - Sender B has `TX_C` (MEDIUM tip)
    ///
    /// `TX_A` is in the mempool, `TX_B` and `TX_C` arrive later after the first flashblock has
    /// started building already.
    ///
    /// - In flashblock 1, `TX_A` gets consumed (`TX_B` unlocks after `TX_A`)
    /// - Only `TX_A` is marked as committed (simulating flashblock timer expiring)
    /// - In flashblock 2, `TX_B` (HIGH tip) should come before `TX_C` (MEDIUM tip)
    ///
    /// Expected: `TX_B` (100 gwei) before `TX_C` (10 gwei) in flashblock 2.
    ///
    /// The upstream reth PR (<https://github.com/paradigmxyz/reth/pull/21765>) that added
    /// `prune_transactions` to the pool trait has been merged. The production fix calls
    /// `pool.prune_transactions` after `mark_committed` between flashblocks, which removes
    /// the already-executed `TX_A` from the pool so the iterator sees the correct priority
    /// ordering. This test simulates that behavior by recreating the pool without `TX_A`
    /// and verifies that `TX_B` (100 gwei) is correctly ordered before `TX_C` (10 gwei).
    #[test]
    fn test_nonce_chain_gating_bug_across_flashblocks() {
        let tx_a = transaction(0, 0, 1_000_000_000); // 1 gwei - LOW
        let tx_b = transaction(0, 1, 100_000_000_000); // 100 gwei - HIGH (depends on TX_A)
        let tx_c = transaction(1, 0, 10_000_000_000); // 10 gwei - MEDIUM
        let mut pool = pending_pool(&[Arc::clone(&tx_a)]);

        // === FLASHBLOCK 1 ===
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        // Simulate: Flashblock 1 starts building
        // Start consuming txns from the txpool
        let first = iterator.next(()).unwrap();
        assert_eq!(*first.hash(), *tx_a.hash(), "First should be TX_A (1 gwei)");
        iterator.mark_current_committed();

        // TX_B and TX_C arrive late, but we have already yielded lower-priority transactions
        // from the iterator, so these do not immediately get added to the best txns
        pool.add_transaction(Arc::clone(&tx_b), 0);
        pool.add_transaction(Arc::clone(&tx_c), 0);
        assert!(iterator.next(()).is_none());

        // Simulate: flashblock 1 is complete after TX_A was executed
        iterator.mark_committed(&[*tx_a.hash()]);
        // Simulate pool.prune_transactions by recreating the pool without TX_A
        let pool = pending_pool(&[Arc::clone(&tx_b), Arc::clone(&tx_c)]);

        // === FLASHBLOCK 2 ===
        // We refresh the iterator with the latest best transactions
        iterator.refresh_iterator(parkable(&pool));

        // TX_A has already been executed, so TX_B (100 gwei) is the best txn and TX_C
        // (10 gwei) the second best
        assert_eq!(drain_without_including(&mut iterator), vec![*tx_b.hash(), *tx_c.hash()]);
    }

    /// Reproduces the nonce-chain queuing bug caused by `prune_transactions`.
    ///
    /// After FB1 prunes executed nonce-0 txs, the pool's on-chain nonce view is stale
    /// (block not sealed), so nonce-1 txs from the same senders land in `queued`
    /// instead of `pending`, making them invisible to FB2+.
    #[tokio::test]
    async fn test_prune_transactions_causes_nonce_chain_queuing() {
        use alloy_primitives::{Address, U256};
        use reth_execution_types::ChangedAccount;
        use reth_transaction_pool::{
            BestTransactionsAttributes, TransactionOrigin, TransactionPool, TransactionPoolExt,
            test_utils::{MockTransaction, testing_pool},
        };

        let pool = testing_pool();

        let senders: Vec<Address> = (0..3).map(|_| Address::random()).collect();

        // All senders submit nonce-0 txs
        for sender in &senders {
            let tx = MockTransaction::eip1559()
                .with_sender(*sender)
                .with_nonce(0)
                .with_gas_limit(21_000)
                .with_priority_fee(5_000_000_000)
                .with_max_fee(100_000_000_000);
            pool.add_transaction(TransactionOrigin::External, tx).await.unwrap();
        }
        assert_eq!(pool.pool_size().pending, 3);

        // Simulate FB1: consume all nonce-0 txs, then prune them
        let best_attrs = BestTransactionsAttributes::new(0, None);
        let mut best_iter = pool.best_transactions_with_attributes(best_attrs);
        let mut executed_hashes = Vec::new();
        for tx in best_iter.by_ref() {
            executed_hashes.push(*tx.hash());
        }
        drop(best_iter);
        assert_eq!(executed_hashes.len(), 3);
        pool.prune_transactions(executed_hashes);
        assert_eq!(pool.pool_size().pending, 0);

        // Senders submit nonce-1 txs (arrive between FB1 and FB2)
        for sender in &senders {
            let tx = MockTransaction::eip1559()
                .with_sender(*sender)
                .with_nonce(1)
                .with_gas_limit(21_000)
                .with_priority_fee(5_000_000_000)
                .with_max_fee(100_000_000_000);
            pool.add_transaction(TransactionOrigin::External, tx).await.unwrap();
        }

        // Bug: nonce-1 txs are queued (nonce gap) because pool still thinks on-chain nonce is 0
        assert_eq!(pool.pool_size().pending, 0, "nonce-1 txs should be queued without fix");
        assert_eq!(pool.pool_size().queued, 3, "nonce-1 txs land in queued due to stale nonce");

        // Fix: update_accounts corrects the pool's nonce view, promoting queued -> pending.
        // U256::MAX balance is fine here — testing_pool has no revm state to read from.
        // Production code uses state.basic(address) for real balances.
        let changed_accounts: Vec<ChangedAccount> = senders
            .iter()
            .map(|&address| ChangedAccount { address, nonce: 1, balance: U256::MAX })
            .collect();
        pool.update_accounts(changed_accounts);
        assert_eq!(pool.pool_size().pending, 3, "nonce-1 txs should be pending after fix");
        assert_eq!(pool.pool_size().queued, 0, "no txs should be queued after fix");

        // FB2's iterator must see all 3 nonce-1 txs
        let mut fb2_iter = pool.best_transactions_with_attributes(best_attrs);
        let mut count = 0;
        while fb2_iter.next().is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    /// A rejected transaction becomes eligible again after the cache TTL expires.
    #[test]
    fn test_rejected_tx_eligible_after_ttl_expiry() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2]);

        // TTL is short, 1ms
        let cache = RejectionCache::new(1000, Duration::from_millis(1));

        // Reject tx_2
        let mut iter1 = BestFlashblocksTxs::new(parkable(&pool), cache.clone());
        assert_eq!(drain_without_including(&mut iter1).len(), 2);
        iter1.mark_rejected(&[tx_2_hash]);

        // Wait for TTL to expire and flush pending evictions
        std::thread::sleep(Duration::from_millis(50));
        cache.run_pending_tasks();

        // New iterator — tx_2 should be back
        let mut iter2 = BestFlashblocksTxs::new(parkable(&pool), cache);
        let seen_hashes = drain_without_including(&mut iter2);
        assert!(seen_hashes.contains(&tx_2_hash), "tx should be eligible again after TTL expiry");
        assert_eq!(seen_hashes.len(), 2, "both txs should appear");
    }

    fn balance_at_least(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn balance_change(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.info.balance = U256::from(new);
        account.original_info_mut().balance = U256::from(old);
        EvmState::from_iter([(address, account)])
    }

    /// Builds the production lane-parking iterator over a snapshot of `pool` whose hold parks the
    /// resting members of `resting`, as the pool does for an enforcing build.
    fn parkable_with_hold(
        pool: &PendingPool<Ordering>,
        hold: Option<Box<dyn BestTransactionsHold<BasePooledTransaction>>>,
    ) -> Parkable {
        ParkableBestPayloadTransactions::new(Box::new(
            ParkedBestTransactions::new(pool.best(), Ordering::coinbase_tip(), 0).with_hold(hold),
        ))
    }

    fn resting_iterator_in_mode(
        pool: &PendingPool<Ordering>,
        mode: RestingPredicateMode,
        resting: RestingPredicates,
    ) -> BestFlashblocksTxs<BasePooledTransaction, Parkable> {
        let resting = SharedRestingPredicates::new(resting);
        let hold = mode.is_enforced().then(|| resting.iterator_hold());
        BestFlashblocksTxs::new(parkable_with_hold(pool, hold), test_rejection_cache())
            .with_resting_predicate_mode(mode)
            .with_resting_predicates(resting)
    }

    fn resting_iterator(
        pool: &PendingPool<Ordering>,
    ) -> BestFlashblocksTxs<BasePooledTransaction, Parkable> {
        resting_iterator_in_mode(
            pool,
            RestingPredicateMode::Enforce,
            RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD),
        )
    }

    /// Starts the next flashblock over a fresh snapshot of `pool`.
    fn refresh(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        pool: &PendingPool<Ordering>,
    ) {
        iterator.set_cancel(CancellationToken::new());
        iterator.refresh_iterator(parkable_with_hold(pool, iterator.iterator_hold()));
    }

    /// Plays the build loop's part for a candidate whose predicate is unsatisfied.
    fn park_unsatisfied(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        transaction: &ValidPoolTransaction<BasePooledTransaction>,
    ) {
        iterator.park_current();
        let predicate = transaction.transaction.validity_predicates().next().unwrap();
        iterator.rest(*transaction.hash(), predicate);
    }

    /// Fills a pool with `resting` validity transactions from distinct senders that outrank
    /// `ordinary` plain transactions from further senders, and returns the ordinary hashes.
    fn resting_cohort(
        resting: u64,
        ordinary: u64,
    ) -> (Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>, Vec<TxHash>) {
        let mut transactions: Vec<_> = (0..resting)
            .map(|sender| validity_transaction(sender, 0, 10, vec![balance_at_least(WATCHED, 1)]))
            .collect();
        let ordinary_hashes = (resting..resting + ordinary)
            .map(|sender| {
                let plain = transaction(sender, 0, 1);
                let hash = *plain.hash();
                transactions.push(plain);
                hash
            })
            .collect();
        (transactions, ordinary_hashes)
    }

    fn rest_all(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        transactions: &[Arc<ValidPoolTransaction<BasePooledTransaction>>],
    ) {
        for transaction in transactions {
            if let Some(predicate) = transaction.transaction.validity_predicates().next() {
                iterator.rest(*transaction.hash(), predicate);
            }
        }
    }

    /// Diagnostics of a flashblock whose build loop stopped on the cancel.
    fn cancelled_diagnostics() -> FlashblockDiagnostics {
        FlashblockDiagnostics { cancelled: true, ..Default::default() }
    }

    /// A flashblock selection context on the test transactions' chain.
    fn selection_context() -> BasePayloadBuilderCtx {
        let genesis = serde_json::from_value(serde_json::json!({
            "config": { "chainId": 1 },
            "gasLimit": "0x1C9C380",
            "timestamp": "0x0"
        }))
        .expect("valid genesis");
        let chain_spec = ChainSpec::builder().chain(1.into()).genesis(genesis).cancun_activated();
        let parent = Header { gas_limit: 30_000_000, timestamp: 0, ..Default::default() };
        BasePayloadBuilderCtx::for_test(
            Arc::new(BaseChainSpec::from(chain_spec.build())),
            Arc::new(SealedHeader::seal_slow(parent)),
        )
    }

    fn cancelled() -> CancellationToken {
        let cancel = CancellationToken::new();
        cancel.cancel();
        cancel
    }

    /// Starts a flashblock cancelled by `cancel` the way the flashblocks builder does, and returns
    /// whether it resumed the previous walk.
    fn begin(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        pool: &PendingPool<Ordering>,
        cancel: CancellationToken,
    ) -> bool {
        iterator.begin_flashblock(cancel, |hold| parkable_with_hold(pool, hold))
    }

    /// Ends a flashblock the way the build loop and the flashblocks builder do, and returns the
    /// resting transactions it held.
    fn finish(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        diagnostics: &FlashblockDiagnostics,
    ) -> u64 {
        let held = iterator.take_resting_stats().parked;
        iterator.finish_flashblock(diagnostics);
        held
    }

    /// A flashblock cancelled while its iterator walks a 50k resting cohort leaves the walk to the
    /// next flashblock, which resumes it instead of re-walking the cohort from the top and fetching
    /// a new pool snapshot: across the cut every resting transaction is held exactly once, and the
    /// ordinary transactions behind the cohort are reached. Restarting the walk every flashblock
    /// never reaches them while each flashblock is cut.
    #[test]
    fn cut_resting_walk_resumes_in_the_next_flashblock() {
        const RESTING: u64 = 50_000;
        let (transactions, ordinary) = resting_cohort(RESTING, 2);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        // Flashblock 1 is cancelled, so its walk stops early.
        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        let held_before_cut = finish(&mut iterator, &FlashblockDiagnostics::default());
        assert_eq!(held_before_cut, u64::from(HOLD_INTERRUPT_INTERVAL));

        // Flashblock 2 resumes the cut walk without fetching from the pool, and reaches the first
        // ordinary transaction without holding any resting transaction a second time.
        let resumed = iterator.begin_flashblock(CancellationToken::new(), |_| {
            panic!("a resumed flashblock must not fetch a new pool iterator")
        });
        assert!(resumed);
        assert_eq!(iterator.next(()).unwrap().hash(), &ordinary[0]);
        iterator.mark_current_committed();
        assert_eq!(
            finish(&mut iterator, &FlashblockDiagnostics::default()),
            RESTING - held_before_cut
        );

        // That flashblock was not cut, so flashblock 3 takes a fresh iterator.
        assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
    }

    /// A resumed walk is interrupted by the cancellation of the flashblock it now serves, not by
    /// that of the flashblock that created its hold: the previous token has already fired, and
    /// checking it would end every resumed walk at its first interrupt check.
    #[test]
    fn resumed_walk_checks_the_current_flashblock_cancel() {
        const RESTING: u64 = 1_000;
        let interval = u64::from(HOLD_INTERRUPT_INTERVAL);
        let (transactions, ordinary) = resting_cohort(RESTING, 1);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), interval);

        // Flashblock 2 is cancelled too, so its walk stops after one more interval.
        assert!(begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), interval);

        // Flashblock 3 runs to its deadline and walks the rest of the cohort.
        assert!(begin(&mut iterator, &pool, CancellationToken::new()));
        assert_eq!(iterator.next(()).unwrap().hash(), &ordinary[0]);
        iterator.mark_current_committed();
        assert_eq!(
            finish(&mut iterator, &FlashblockDiagnostics::default()),
            RESTING - 2 * interval
        );
    }

    /// A candidate yielded by a cancelled flashblock that the build loop dropped without a
    /// decision is offered again, before its nonce descendant, when the next flashblock resumes
    /// the cut walk.
    #[test]
    fn resumed_walk_reoffers_the_abandoned_candidate_before_its_descendant() {
        const RESTING: u64 = 1_000;
        let (mut transactions, _ordinary) = resting_cohort(RESTING, 1);
        let parent = transaction(RESTING + 1, 0, 5);
        let child = transaction(RESTING + 1, 1, 5);
        transactions.extend([Arc::clone(&parent), Arc::clone(&child)]);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        // The walk passes the cohort, then the build loop sees the cancel and drops the parent.
        let cancel = CancellationToken::new();
        assert!(!begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(iterator.next(()).unwrap().hash(), parent.hash());
        cancel.cancel();
        finish(&mut iterator, &cancelled_diagnostics());

        assert!(begin(&mut iterator, &pool, CancellationToken::new()));
        assert_eq!(iterator.next(()).unwrap().hash(), parent.hash());
        iterator.mark_current_committed();
        assert_eq!(iterator.next(()).unwrap().hash(), child.hash());
    }

    /// A transaction the build loop parked without resting it, such as one deferred past the
    /// predicate-evaluation budget, is offered again when the next flashblock resumes the cut walk,
    /// as a fresh iterator would offer it.
    #[test]
    fn resumed_walk_reoffers_build_loop_deferrals() {
        const RESTING: u64 = 1_000;
        let (mut transactions, _ordinary) = resting_cohort(RESTING, 1);
        let deferred =
            validity_transaction(RESTING + 1, 0, 100, vec![balance_at_least(WATCHED, 1)]);
        transactions.push(Arc::clone(&deferred));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions[..RESTING as usize]);
        let cancel = CancellationToken::new();
        assert!(!begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(*iterator.next(()).unwrap().hash(), *deferred.hash());
        iterator.park_current();

        // The cancel then cuts the walk in the resting cohort.
        cancel.cancel();
        assert!(iterator.next(()).is_none());
        finish(&mut iterator, &FlashblockDiagnostics::default());

        assert!(begin(&mut iterator, &pool, CancellationToken::new()));
        assert_eq!(*iterator.next(()).unwrap().hash(), *deferred.hash());
    }

    /// A transaction the build loop parked and rested stays parked when the next flashblock resumes
    /// the cut walk, and a commit that wakes it promotes it, as a fresh iterator's hold would.
    #[test]
    fn resumed_walk_keeps_build_loop_rested_transactions_parked_until_woken() {
        const RESTING: u64 = 1_000;
        let (mut transactions, ordinary) = resting_cohort(RESTING, 1);
        let predicate = balance_at_least(WATCHED, 1);
        let rested = validity_transaction(RESTING + 1, 0, 100, vec![predicate.clone()]);
        transactions.push(Arc::clone(&rested));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions[..RESTING as usize]);
        let cancel = CancellationToken::new();
        assert!(!begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(*iterator.next(()).unwrap().hash(), *rested.hash());
        iterator.park_current();
        iterator.rest(*rested.hash(), &predicate);
        cancel.cancel();
        assert!(iterator.next(()).is_none());
        finish(&mut iterator, &FlashblockDiagnostics::default());

        assert!(begin(&mut iterator, &pool, CancellationToken::new()));
        assert_eq!(*iterator.next(()).unwrap().hash(), ordinary[0]);
        iterator.mark_current_committed();
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert_eq!(*iterator.next(()).unwrap().hash(), *rested.hash());
    }

    /// A cut walk whose flashblock skipped transactions for a limit a later flashblock may satisfy
    /// is not resumed, so the next flashblock's fresh iterator offers those transactions again.
    #[test]
    fn cut_walk_after_a_retryable_limit_rejection_refreshes() {
        let (transactions, _ordinary) = resting_cohort(1_000, 1);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);
        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        finish(&mut iterator, &FlashblockDiagnostics { txs_rejected_gas: 1, ..Default::default() });

        assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
    }

    /// A cancel that lands right after a walk past only a few held transactions, here one held
    /// head and its lane-blocked descendants, does not resume the walk: there is no long resting
    /// run to save re-walking.
    #[test]
    fn cut_walk_behind_few_holds_refreshes() {
        let mut transactions =
            vec![validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)])];
        transactions.extend(
            (1..=2 * u64::from(HOLD_INTERRUPT_INTERVAL)).map(|nonce| transaction(0, nonce, 10)),
        );
        let plain = transaction(1, 0, 1);
        transactions.push(Arc::clone(&plain));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions[..1]);
        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert_eq!(iterator.next(()).unwrap().hash(), plain.hash());
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), 1);

        assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
    }

    /// Runs `f` against a local recorder and returns the rendered metrics.
    fn rendered_metrics(f: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    /// Starts a block over a 1,000-member resting cohort whose first flashblock is cancelled, so
    /// its walk is cut after one interrupt interval of holds.
    fn cut_cohort_walk()
    -> (PendingPool<Ordering>, BestFlashblocksTxs<BasePooledTransaction, Parkable>) {
        let (transactions, _ordinary) = resting_cohort(1_000, 1);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);
        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        (pool, iterator)
    }

    /// A cut walk the next flashblock continues counts as resumed and never as reset.
    #[test]
    fn resumed_walk_is_counted_as_resumed() {
        let rendered = rendered_metrics(|| {
            let (pool, mut iterator) = cut_cohort_walk();
            finish(&mut iterator, &cancelled_diagnostics());
            assert!(begin(&mut iterator, &pool, CancellationToken::new()));
            iterator.finish_block();
        });

        assert!(rendered.contains("base_builder_resting_walk_resumed_total 1"), "{rendered}");
        assert!(!rendered.contains("resting_walk_reset_total"), "{rendered}");
    }

    /// A walk cut in the last flashblock of a block would have been resumed, but the block ends
    /// and the next block starts a fresh iterator, so it counts once as a block-end reset.
    #[test]
    fn walk_cut_in_the_last_flashblock_is_counted_as_a_block_end_reset() {
        let rendered = rendered_metrics(|| {
            let (_pool, mut iterator) = cut_cohort_walk();
            finish(&mut iterator, &cancelled_diagnostics());
            iterator.finish_block();
            iterator.finish_block();
        });

        assert!(
            rendered.contains("base_builder_resting_walk_reset_total{reason=\"block_end\"} 1"),
            "{rendered}"
        );
        assert!(!rendered.contains("resting_walk_resumed_total"), "{rendered}");
    }

    /// A cut walk dropped because its flashblock skipped transactions for a retryable limit counts
    /// as a limit-rejection reset, not a block-end reset when the block later ends.
    #[test]
    fn cut_walk_dropped_for_a_retryable_limit_rejection_is_counted() {
        let rendered = rendered_metrics(|| {
            let (pool, mut iterator) = cut_cohort_walk();
            finish(
                &mut iterator,
                &FlashblockDiagnostics { txs_rejected_gas: 1, ..cancelled_diagnostics() },
            );
            assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
            iterator.finish_block();
        });

        assert!(
            rendered
                .contains("base_builder_resting_walk_reset_total{reason=\"limit_rejection\"} 1"),
            "{rendered}"
        );
        assert!(!rendered.contains("block_end"), "{rendered}");
    }

    /// A walk cut behind too few holds to resume counts as a few-holds reset.
    #[test]
    fn cut_walk_behind_few_holds_is_counted() {
        let mut transactions =
            vec![validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)])];
        transactions.extend(
            (1..=2 * u64::from(HOLD_INTERRUPT_INTERVAL)).map(|nonce| transaction(0, nonce, 10)),
        );
        transactions.push(transaction(1, 0, 1));
        let pool = pending_pool(&transactions);
        let rendered = rendered_metrics(|| {
            let mut iterator = resting_iterator(&pool);
            rest_all(&mut iterator, &transactions[..1]);
            assert!(!begin(&mut iterator, &pool, cancelled()));
            assert!(iterator.next(()).is_some());
            finish(&mut iterator, &cancelled_diagnostics());
            assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
            iterator.finish_block();
        });

        assert!(
            rendered.contains("base_builder_resting_walk_reset_total{reason=\"few_holds\"} 1"),
            "{rendered}"
        );
        assert!(!rendered.contains("block_end"), "{rendered}");
    }

    /// A cancel the hold never checks, because the walk finishes its resting run in fewer than
    /// [`HOLD_INTERRUPT_INTERVAL`] holds, still leaves the walk to the next flashblock: the build
    /// loop sees it on the candidate the iterator yields next and stops without deciding on it.
    #[test]
    fn cancel_after_the_last_stop_check_still_cuts_the_walk() {
        // 70 resting, a plain transaction, then 30 resting: the second run holds 30 in one `next`
        // call, too few for the hold to check the cancel.
        let mut transactions: Vec<_> = (0..70)
            .map(|sender| validity_transaction(sender, 0, 30, vec![balance_at_least(WATCHED, 1)]))
            .collect();
        let between = transaction(70, 0, 20);
        transactions.push(Arc::clone(&between));
        transactions.extend(
            (71..101).map(|sender| {
                validity_transaction(sender, 0, 10, vec![balance_at_least(WATCHED, 1)])
            }),
        );
        let ordinary = transaction(101, 0, 1);
        transactions.push(Arc::clone(&ordinary));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        let cancel = CancellationToken::new();
        assert!(!begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(iterator.next(()).unwrap().hash(), between.hash());
        iterator.mark_current_committed();
        cancel.cancel();
        assert_eq!(iterator.next(()).unwrap().hash(), ordinary.hash());
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), 100);

        assert!(begin(&mut iterator, &pool, CancellationToken::new()));
        assert_eq!(iterator.next(()).unwrap().hash(), ordinary.hash());
    }

    /// A cancel that lands while the flashblock builds ordinary transactions past the resting set
    /// does not cut the walk, even after the flashblock held many resting transactions earlier:
    /// there is no resting run left to save re-walking, so the next flashblock refreshes.
    #[test]
    fn cancel_while_building_past_the_resting_set_refreshes() {
        const RESTING: u64 = 100;
        let (transactions, ordinary) = resting_cohort(RESTING, 2);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        let cancel = CancellationToken::new();
        assert!(!begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(iterator.next(()).unwrap().hash(), &ordinary[0]);
        iterator.mark_current_committed();
        assert_eq!(iterator.next(()).unwrap().hash(), &ordinary[1]);
        cancel.cancel();
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), RESTING);

        assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
    }

    /// A resume chain ends once a cut flashblock held fewer than
    /// [`RESTING_WALK_RESUME_MIN_HOLDS`] resting transactions itself, however many the chain held
    /// before, so the next flashblock refreshes and sees the current pool.
    #[test]
    fn resume_chain_stops_once_a_flashblock_holds_few() {
        const RESTING: u64 = 1_000;
        let resting =
            |sender, tip| validity_transaction(sender, 0, tip, vec![balance_at_least(WATCHED, 1)]);
        // In priority order: the cohort, a plain transaction, 10 resting, a plain transaction, 5
        // resting, a plain transaction, 5 resting, and a last plain transaction.
        let mut transactions: Vec<_> = (0..RESTING).map(|sender| resting(sender, 30)).collect();
        let first = transaction(RESTING, 0, 20);
        transactions.push(Arc::clone(&first));
        transactions.extend((RESTING + 1..RESTING + 11).map(|sender| resting(sender, 15)));
        let second = transaction(RESTING + 11, 0, 12);
        transactions.push(Arc::clone(&second));
        transactions.extend((RESTING + 12..RESTING + 17).map(|sender| resting(sender, 10)));
        let third = transaction(RESTING + 17, 0, 8);
        transactions.push(Arc::clone(&third));
        transactions.extend((RESTING + 18..RESTING + 23).map(|sender| resting(sender, 5)));
        let last = transaction(RESTING + 23, 0, 1);
        transactions.push(Arc::clone(&last));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);

        // Flashblock 1 is cut in the cohort.
        assert!(!begin(&mut iterator, &pool, cancelled()));
        assert!(iterator.next(()).is_none());
        finish(&mut iterator, &FlashblockDiagnostics::default());

        // Flashblock 2 finishes the cohort and is cut right after the run of 10.
        let cancel = CancellationToken::new();
        assert!(begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(iterator.next(()).unwrap().hash(), first.hash());
        iterator.mark_current_committed();
        assert_eq!(iterator.next(()).unwrap().hash(), second.hash());
        cancel.cancel();
        assert_eq!(
            finish(&mut iterator, &cancelled_diagnostics()),
            RESTING - u64::from(HOLD_INTERRUPT_INTERVAL) + 10
        );

        // Flashblock 3 resumes and is cut right after the second run of 5, holding only the two
        // runs of 5.
        let cancel = CancellationToken::new();
        assert!(begin(&mut iterator, &pool, cancel.clone()));
        assert_eq!(iterator.next(()).unwrap().hash(), second.hash());
        iterator.mark_current_committed();
        assert_eq!(iterator.next(()).unwrap().hash(), third.hash());
        iterator.mark_current_committed();
        assert_eq!(iterator.next(()).unwrap().hash(), last.hash());
        cancel.cancel();
        assert_eq!(finish(&mut iterator, &cancelled_diagnostics()), 10);

        assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
    }

    /// The build loop drives the resume seam end to end: a flashblock cancelled while its hold
    /// walks the resting cohort reports itself cancelled, and the next flashblock's selection
    /// continues the same walk under its own cancellation and considers the transactions behind
    /// the cohort.
    #[test]
    fn build_loop_resumes_a_walk_its_hold_stopped() {
        const RESTING: u64 = 1_000;
        let (transactions, ordinary) = resting_cohort(RESTING, 2);
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);
        rest_all(&mut iterator, &transactions);
        let db = StateProviderDatabase::new(NoopProvider::default().into_evm_state_provider());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut select =
            |ctx: &BasePayloadBuilderCtx,
             iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>| {
                ctx.execute_best_transactions(
                    &mut ExecutionInfo::default(),
                    &mut BlockDeferrals::default(),
                    &mut state,
                    iterator,
                    &ResourceLimits::default(),
                )
                .expect("selection should succeed")
            };

        // The flashblock's cancel fires before its walk past the cohort ends.
        let cut_ctx = selection_context();
        assert!(!begin(&mut iterator, &pool, cut_ctx.cancel.clone()));
        cut_ctx.cancel.cancel();
        let cut = select(&cut_ctx, &mut iterator);
        assert!(cut.cancelled);
        assert_eq!(cut.txs_considered, 0);
        iterator.finish_flashblock(&cut);

        let next_ctx = selection_context();
        let resumed = iterator.begin_flashblock(next_ctx.cancel.clone(), |_| {
            panic!("a resumed flashblock must not fetch a new pool iterator")
        });
        assert!(resumed);
        let next = select(&next_ctx, &mut iterator);
        assert!(!next.cancelled);
        assert_eq!(next.txs_considered, ordinary.len() as u64);
    }

    /// Without the hold, under shadow or off, a cut flashblock never resumes.
    #[test]
    fn unenforced_modes_never_resume() {
        for mode in [RestingPredicateMode::Shadow, RestingPredicateMode::Off] {
            let (transactions, _ordinary) = resting_cohort(1_000, 1);
            let pool = pending_pool(&transactions);
            let mut iterator = resting_iterator_in_mode(
                &pool,
                mode,
                RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD),
            );
            rest_all(&mut iterator, &transactions);
            // The build loop sees the cancel on the first candidate and stops without deciding.
            assert!(!begin(&mut iterator, &pool, cancelled()));
            assert!(iterator.next(()).is_some());
            finish(&mut iterator, &cancelled_diagnostics());

            assert!(!begin(&mut iterator, &pool, CancellationToken::new()));
        }
    }

    #[test]
    fn resting_transaction_is_held_back_until_its_state_changes() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let unrelated = transaction(1, 0, 5);
        let trigger = transaction(2, 0, 4);
        let low = transaction(3, 0, 1);
        let pool = pending_pool(&[
            Arc::clone(&resting),
            Arc::clone(&unrelated),
            Arc::clone(&trigger),
            Arc::clone(&low),
        ]);
        let mut iterator = resting_iterator(&pool);

        // Flashblock 1: the build loop finds the predicate unsatisfied.
        let first = iterator.next(()).unwrap();
        assert_eq!(*first.hash(), *resting.hash());
        park_unsatisfied(&mut iterator, &first);

        // Flashblock 2: the resting transaction is not yielded, and a commit to unrelated state
        // does not release it.
        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *unrelated.hash());
        iterator.record_committed_state(&balance_change(UNRELATED, 0, 1));
        iterator.mark_current_committed();
        assert_eq!(iterator.take_resting_stats().parked, 1);

        // A commit to the watched balance releases it ahead of lower-priority candidates.
        assert_eq!(*iterator.next(()).unwrap().hash(), *trigger.hash());
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *low.hash());
    }

    #[test]
    fn cancelled_flashblock_stops_walking_past_resting_transactions() {
        let members = u64::from(HOLD_INTERRUPT_INTERVAL) + 1;
        let mut transactions = (0..members)
            .map(|sender| validity_transaction(sender, 0, 10, vec![balance_at_least(WATCHED, 1)]))
            .collect::<Vec<_>>();
        let plain = transaction(members, 0, 1);
        transactions.push(Arc::clone(&plain));
        let pool = pending_pool(&transactions);
        let mut iterator = resting_iterator(&pool);

        // Flashblock 1: the build loop finds every validity candidate unsatisfied.
        for _ in 0..members {
            let candidate = iterator.next(()).unwrap();
            park_unsatisfied(&mut iterator, &candidate);
        }

        // Flashblock 2 is cancelled before its walk past the resting members reaches `plain`, so
        // the iterator returns control after one check interval instead of finishing the walk.
        iterator.set_cancel(cancelled());
        iterator.refresh_iterator(parkable_with_hold(&pool, iterator.iterator_hold()));
        assert!(iterator.next(()).is_none());
        assert_eq!(iterator.take_resting_stats().parked, u64::from(HOLD_INTERRUPT_INTERVAL));

        // Flashblock 3 runs to its deadline and walks past every member.
        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *plain.hash());
        assert_eq!(iterator.take_resting_stats().parked, members);
    }

    #[test]
    fn nonce_descendant_waits_for_resting_parent() {
        let parent = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let child = transaction(0, 1, 100);
        let other = transaction(1, 0, 1);
        let pool = pending_pool(&[Arc::clone(&parent), Arc::clone(&child), Arc::clone(&other)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *other.hash());
        iterator.mark_current_committed();
        assert!(iterator.next(()).is_none());

        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert_eq!(*iterator.next(()).unwrap().hash(), *parent.hash());
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *child.hash());
    }

    /// A stale resting member that another build rejected is held back with its nonce lane, as the
    /// rejection cache would exclude it, and the rejection cache excludes it once it is released.
    #[test]
    fn rejected_resting_transaction_and_its_lane_stay_excluded() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let child = transaction(0, 1, 100);
        let other = transaction(1, 0, 1);
        let pool = pending_pool(&[Arc::clone(&resting), Arc::clone(&child), Arc::clone(&other)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        iterator.mark_rejected(&[*resting.hash()]);

        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *other.hash());
        iterator.mark_current_committed();
        assert!(iterator.next(()).is_none());
        assert_eq!(iterator.take_resting_stats().parked, 1);

        // Once a state change releases it, the rejection cache excludes it and its descendant.
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        refresh(&mut iterator, &pool);
        assert!(iterator.next(()).is_none());
    }

    /// A transaction parked by the build loop in the current flashblock is woken by the build
    /// loop's own predicate index, which re-evaluates it first, so the iterator does not promote
    /// it. It no longer rests, so the next flashblock yields it for evaluation.
    #[test]
    fn transaction_parked_by_build_loop_is_not_promoted_by_a_wake() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 2)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert!(iterator.next(()).is_none());

        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn resting_work_is_reported_when_the_iterator_is_exhausted() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        assert_eq!(iterator.take_resting_stats(), RestingStats::default());

        refresh(&mut iterator, &pool);
        assert!(iterator.next(()).is_none());
        let stats = iterator.take_resting_stats();
        assert_eq!(stats.parked, 1);
        assert!(!stats.duration.is_zero());
    }

    #[test]
    fn latest_rested_predicate_replaces_the_previous_one() {
        let resting = validity_transaction(
            0,
            0,
            10,
            vec![balance_at_least(WATCHED, 1), balance_at_least(UNRELATED, 1)],
        );
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        let unrelated = first.transaction.validity_predicates().nth(1).unwrap();
        iterator.rest(*resting.hash(), unrelated);

        refresh(&mut iterator, &pool);
        assert!(iterator.next(()).is_none());
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert!(iterator.next(()).is_none());
        iterator.record_committed_state(&balance_change(UNRELATED, 0, 1));
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn readded_hash_without_the_rested_predicate_is_yielded() {
        let original = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let readded = validity_transaction(0, 0, 10, vec![balance_at_least(UNRELATED, 1)]);
        assert_eq!(*original.hash(), *readded.hash());
        let mut iterator = resting_iterator(&pending_pool(&[original]));

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        refresh(&mut iterator, &pending_pool(&[Arc::clone(&readded)]));
        assert_eq!(*iterator.next(()).unwrap().hash(), *readded.hash());
    }

    #[test]
    fn flashblock_index_predicate_does_not_rest() {
        let resting = validity_transaction(
            0,
            0,
            10,
            vec![ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(3),
            }],
        );
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        refresh(&mut iterator, &pool);
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn shadow_mode_tracks_resting_transactions_without_holding_them_back() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator_in_mode(
            &pool,
            RestingPredicateMode::Shadow,
            RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD),
        );

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        refresh(&mut iterator, &pool);
        let yielded = iterator.next(()).unwrap();
        assert_eq!(*yielded.hash(), *resting.hash());
        assert!(iterator.is_resting(*resting.hash(), yielded.transaction.validity_conditions()));
    }

    #[test]
    fn off_mode_does_not_track_resting_transactions() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let predicate = resting.transaction.validity_predicates().next().unwrap();
        let mut iterator = BestFlashblocksTxs::new(
            parkable(&pending_pool(&[Arc::clone(&resting)])),
            test_rejection_cache(),
        );

        iterator.rest(*resting.hash(), predicate);

        assert!(!iterator.is_resting(*resting.hash(), resting.transaction.validity_conditions()));
    }

    /// Finishes the block `iterator` built and returns the next block's iterator over `pool`,
    /// carrying the resting set.
    fn next_block_iterator(
        mut iterator: BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        pool: &PendingPool<Ordering>,
        mode: RestingPredicateMode,
    ) -> BestFlashblocksTxs<BasePooledTransaction, Parkable> {
        const FINISHED: B256 = B256::repeat_byte(0x01);
        let mut resting = iterator.take_resting_predicates();
        resting.end_block(FINISHED);
        resting.begin_block(FINISHED, 1, Some(&TransitionState::default()));
        resting_iterator_in_mode(pool, mode, resting)
    }

    #[test]
    fn carried_resting_transaction_is_held_back_until_its_state_changes() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let trigger = transaction(1, 0, 5);
        let pool = pending_pool(&[Arc::clone(&resting), Arc::clone(&trigger)]);
        let mut iterator = resting_iterator(&pool);
        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        let mut iterator = next_block_iterator(iterator, &pool, RestingPredicateMode::Enforce);

        assert_eq!(*iterator.next(()).unwrap().hash(), *trigger.hash());
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
        let stats = iterator.take_resting_stats();
        assert_eq!((stats.parked, stats.carried_parked), (1, 1));
    }

    #[test]
    fn shadow_mode_carries_resting_transactions_without_holding_them_back() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator_in_mode(
            &pool,
            RestingPredicateMode::Shadow,
            RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD),
        );
        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        let mut iterator = next_block_iterator(iterator, &pool, RestingPredicateMode::Shadow);

        let yielded = iterator.next(()).unwrap();
        assert_eq!(*yielded.hash(), *resting.hash());
        assert!(iterator.is_resting(*resting.hash(), yielded.transaction.validity_conditions()));
    }

    #[test]
    fn released_transaction_is_not_carried_into_the_next_block() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);
        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        // A later evaluation in the same block finds the predicate satisfied.
        iterator.release(*resting.hash());

        let mut iterator = next_block_iterator(iterator, &pool, RestingPredicateMode::Enforce);

        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }
}
