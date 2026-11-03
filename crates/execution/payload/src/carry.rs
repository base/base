//! Carries a native build's pool iterator, after it walked past held resting validity
//! transactions, into the build of the next block.
//!
//! From Denim on, the native builder builds each block once, and a resting set that takes longer
//! than one block to walk is cut by the block's finalization request before the walk reaches the
//! transactions ranked below it. A fresh iterator per block walks the set from the top again, so
//! those transactions are never reached. [`CarriedWalkStore`] keeps the iterator, and the next
//! block's build continues it when that block is a child of the block the build sealed. An
//! iterator that reached the transactions below the set is carried too, so the following block
//! does not walk the set again.

use std::{
    any::Any,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use alloy_primitives::{Address, B256, TxHash, map::B256Set};
use parking_lot::Mutex;
use reth_payload_util::PayloadTransactions;
use reth_tasks::CancelOnDrop;

use crate::{HoldInterrupt, ParkablePayloadTransactions, SharedRestingPredicates, ValidityMetrics};

/// Blocks since a carried iterator was created after which the next build takes a fresh one.
///
/// A carried iterator keeps the pool snapshot of the build that created it. Transactions that
/// arrive since and rank above the walk's position, transactions whose fees rose past the cut
/// block's base fee, and replacements are not offered until a build takes a fresh iterator, and
/// lanes a build invalidated for state-dependent reasons, such as an unaffordable tip, stay
/// invalid. Ten 200 ms Denim blocks bound that staleness to about two seconds, the same as one
/// flashblocks block, while a resting set that takes several blocks to walk is still walked once.
///
/// Staleness never makes a block invalid: every candidate a carried iterator yields is evaluated
/// and executed against the new block's state, which rejects mined nonces, fee caps below the new
/// base fee and unsatisfied predicates. A held member that leaves the resting set, for example
/// because the start of a block released it or the unseen sweep dropped it, is offered again.
pub const MAX_CARRIED_ITERATOR_BLOCKS: u64 = 10;

/// Resting transactions a build must have held itself, in the block whose finalization cut its
/// walk, before its pool iterator is carried into the next block.
///
/// A build that held fewer had little resting walk left to save, for example because a carried
/// walk already passed the resting set, so the next build takes a fresh iterator and sees the
/// current pool.
pub const CARRIED_WALK_MIN_HOLDS: u64 = 64;

/// Why a native build did not continue a walk past held resting transactions.
///
/// Counted, by `native_resting_walk_reset_total`, for a carried walk the next build cannot
/// continue and for a walk cut by its block's finalization that is not carried. A continued walk
/// is counted by `native_resting_walk_resumed_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarriedWalkReset {
    /// The iterator was created [`MAX_CARRIED_ITERATOR_BLOCKS`] or more blocks ago.
    BlockLimit,
    /// The next build was not on the block the cut build sealed, such as after a reorg or when
    /// another payload was chosen for that height.
    Reorg,
    /// Any other reason: the cut build held fewer than [`CARRIED_WALK_MIN_HOLDS`] resting
    /// transactions itself or was cancelled, or the carried walk was stored by a build
    /// with another iterator type.
    Other,
}

impl CarriedWalkReset {
    /// Returns the metric label value of this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockLimit => "block_limit",
            Self::Reorg => "reorg",
            Self::Other => "other",
        }
    }

    /// Counts one cut walk reset for this reason.
    pub fn record(self) {
        ValidityMetrics::native_resting_walk_reset_total(self.as_str()).increment(1);
    }
}

/// Observes the cancellation of the payload job currently driving a pool iterator, which may be
/// a later job than the one that created the iterator, and records whether it cut the walk.
#[derive(Debug, Clone, Default)]
pub struct WalkWatch {
    job: Arc<Mutex<Weak<CancelOnDrop>>>,
    cut: Arc<AtomicBool>,
}

impl WalkWatch {
    /// Points the watch at `job`, the cancellation owned by the build that now drives the
    /// iterator. Holding it weakly keeps the build the only owner, because dropping a
    /// `CancelOnDrop` clone cancels the job.
    pub fn watch(&self, job: &Arc<CancelOnDrop>) {
        *self.job.lock() = Arc::downgrade(job);
    }

    /// Returns an interrupt for a hold that stops the walk once the watched job is cancelled,
    /// asked to finalize, or finished, and records that it cut the walk. It takes the lock once
    /// per [`base_execution_txpool::HOLD_INTERRUPT_INTERVAL`] holds.
    pub fn interrupt(&self) -> HoldInterrupt {
        let watch = self.clone();
        Arc::new(move || {
            let interrupted =
                watch.job.lock().upgrade().is_none_or(|cancel| cancel.is_interrupted());
            if interrupted {
                watch.cut.store(true, AtomicOrdering::Relaxed);
            }
            interrupted
        })
    }

    /// Returns and clears whether the interrupt cut the walk.
    pub fn take_cut(&self) -> bool {
        self.cut.swap(false, AtomicOrdering::Relaxed)
    }
}

/// Wraps a build's pool iterator to record what a carried continuation must offer again: the
/// candidate a stopped build never decided on, the transactions the build loop parked, and those
/// it skipped for a block-scope limit.
///
/// Parks are recorded only when the build may carry its walk, which needs enforced resting
/// predicates, so other builds pay one branch per park.
#[derive(Debug)]
pub struct CarryingTransactions<Txs> {
    inner: Txs,
    resting: Option<SharedRestingPredicates>,
    current: Option<TxHash>,
    parked_by_build: B256Set,
    seen_hold_runs: u64,
    current_after_hold: bool,
}

impl<Txs> CarryingTransactions<Txs>
where
    Txs: ParkablePayloadTransactions,
{
    /// Wraps `inner`, whose hold parks members of `resting` when the build enforces resting
    /// predicates.
    pub fn new(inner: Txs, resting: Option<SharedRestingPredicates>) -> Self {
        let seen_hold_runs = resting.as_ref().map_or(0, SharedRestingPredicates::hold_runs);
        Self {
            inner,
            resting,
            current: None,
            parked_by_build: B256Set::default(),
            seen_hold_runs,
            current_after_hold: false,
        }
    }

    /// Returns whether the build loop stopped on a candidate it never decided on that the
    /// iterator reached right after held transactions, which cuts the walk even when the stop
    /// came after the hold's last interrupt check.
    pub const fn stopped_after_hold(&self) -> bool {
        self.current.is_some() && self.current_after_hold
    }

    /// Ends the block of a build whose iterator is carried: members the build loop rested in this
    /// block are handed to the hold, so a commit or a release in a later block promotes them.
    /// Call before the block's resting set is taken.
    pub fn carry(&mut self) {
        if let Some(resting) = &self.resting {
            resting.hold_rested_members(&mut self.parked_by_build);
        }
    }

    /// Continues the walk in a new block: offers again, at their priority position, the
    /// candidate the cut build never decided on, the transactions its build loop parked, and the
    /// `released` held members, which left the resting set at the start of the new block.
    pub fn resume(&mut self, released: impl IntoIterator<Item = TxHash>) {
        if let Some(transaction_hash) = self.current.take() {
            self.inner.park_current();
            self.inner.promote(transaction_hash);
        }
        for transaction_hash in self.parked_by_build.drain().chain(released) {
            self.inner.promote(transaction_hash);
        }
        self.seen_hold_runs = self.resting.as_ref().map_or(0, SharedRestingPredicates::hold_runs);
        self.current_after_hold = false;
    }
}

impl<Txs> PayloadTransactions for &mut CarryingTransactions<Txs>
where
    Txs: ParkablePayloadTransactions,
{
    type Transaction = Txs::Transaction;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        let transaction = self.inner.next(ctx)?;
        if let Some(resting) = &self.resting {
            let hold_runs = resting.hold_runs();
            self.current_after_hold = hold_runs != self.seen_hold_runs;
            self.seen_hold_runs = hold_runs;
        }
        self.current = Some(*transaction.hash());
        Some(transaction)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.current = None;
        self.inner.mark_invalid(sender, nonce);
    }
}

impl<Txs> ParkablePayloadTransactions for &mut CarryingTransactions<Txs>
where
    Txs: ParkablePayloadTransactions,
{
    type Pooled = Txs::Pooled;

    fn park_current(&mut self) {
        if let Some(transaction_hash) = self.current.take()
            && self.resting.is_some()
        {
            self.parked_by_build.insert(transaction_hash);
        }
        self.inner.park_current();
    }

    fn mark_current_committed(&mut self) {
        self.current = None;
        self.inner.mark_current_committed();
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.parked_by_build.remove(&transaction_hash);
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.parked_by_build.remove(&transaction_hash);
        self.inner.discard_parked(transaction_hash)
    }

    /// Parks the candidate instead of invalidating its lane when the build may carry its walk,
    /// so a carried continuation offers it again in the next block, as a fresh iterator would.
    /// Its lane stays occupied, so its nonce descendants wait behind it.
    fn skip_retryable(&mut self, sender: Address, nonce: u64, replay_independent: bool) {
        if self.resting.is_some() {
            self.park_current();
        } else {
            self.current = None;
            self.inner.skip_retryable(sender, nonce, replay_independent);
        }
    }
}

/// A walk past held resting transactions waiting for the build of the next block.
#[derive(Debug)]
pub struct CarriedWalk<Txs> {
    /// The cut iterator.
    pub transactions: CarryingTransactions<Txs>,
    /// The resting set handle the iterator's hold parks members of.
    pub resting: SharedRestingPredicates,
    /// The watch the iterator's hold is interrupted through.
    pub watch: WalkWatch,
    /// Block whose build created the iterator.
    pub first_block: u64,
    /// Hash of the block the cut build sealed.
    pub block_hash: B256,
    /// Number of the block the cut build sealed.
    pub block_number: u64,
}

/// Holds one builder's carried walk between payload jobs.
///
/// The walk is stored type-erased, because the builder struct does not name its pool's
/// transaction type, and a build that takes it with a different iterator type finds nothing.
/// Taking the walk empties the store, so concurrent jobs never share an iterator, and a job that
/// fails or is abandoned never puts it back.
#[derive(Clone, Default)]
pub struct CarriedWalkStore {
    walk: Arc<Mutex<Option<Box<dyn Any + Send>>>>,
}

impl std::fmt::Debug for CarriedWalkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarriedWalkStore").field("carried", &self.walk.lock().is_some()).finish()
    }
}

impl CarriedWalkStore {
    /// Stores the walk of the build that sealed `walk.block_hash`.
    pub fn put<Txs: Send + 'static>(&self, walk: CarriedWalk<Txs>) {
        *self.walk.lock() = Some(Box::new(walk));
    }

    /// Takes the stored walk for the build of `block_number` on `parent_hash`.
    ///
    /// Returns it only when that block is the child of the block the cut build sealed and the
    /// iterator was created fewer than [`MAX_CARRIED_ITERATOR_BLOCKS`] blocks earlier. A stored
    /// walk that fails either check is dropped and its reset counted.
    pub fn take<Txs: 'static>(
        &self,
        parent_hash: B256,
        block_number: u64,
    ) -> Option<CarriedWalk<Txs>> {
        let stored = self.walk.lock().take()?;
        let Ok(walk) = stored.downcast::<CarriedWalk<Txs>>() else {
            CarriedWalkReset::Other.record();
            return None;
        };
        if walk.block_hash != parent_hash || walk.block_number.checked_add(1) != Some(block_number)
        {
            CarriedWalkReset::Reorg.record();
            return None;
        }
        if block_number.saturating_sub(walk.first_block) >= MAX_CARRIED_ITERATOR_BLOCKS {
            CarriedWalkReset::BlockLimit.record();
            return None;
        }
        Some(*walk)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_execution_txpool::BasePooledTransaction;
    use reth_transaction_pool::ValidPoolTransaction;

    use super::*;
    use crate::{
        DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, NoopPayloadTransactions, RestingPredicates,
    };

    type Noop = NoopPayloadTransactions<Arc<ValidPoolTransaction<BasePooledTransaction>>>;

    const SEALED: B256 = B256::repeat_byte(0x07);

    /// A walk created by the build of `first_block` and cut by the build that sealed `SEALED` as
    /// `block_number`.
    fn walk(first_block: u64, block_number: u64) -> CarriedWalk<Noop> {
        let resting = SharedRestingPredicates::new(RestingPredicates::new(
            DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
        ));
        CarriedWalk {
            transactions: CarryingTransactions::new(Noop::default(), Some(resting.clone())),
            resting,
            watch: WalkWatch::default(),
            first_block,
            block_hash: SEALED,
            block_number,
        }
    }

    fn rendered_metrics(f: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    /// The build of the sealed block's child takes the walk, and the store is then empty.
    #[test]
    fn child_of_the_sealed_block_takes_the_walk() {
        let store = CarriedWalkStore::default();
        store.put(walk(1, 1));

        assert!(store.take::<Noop>(SEALED, 2).is_some());
        assert!(store.take::<Noop>(SEALED, 2).is_none());
    }

    /// A build on any other block, such as a sibling of the sealed block after a reorg or the
    /// same parent at a skipped height, resets the walk.
    #[test]
    fn build_off_the_sealed_block_resets_the_walk() {
        for (parent_hash, block_number) in [(B256::repeat_byte(0x08), 2), (SEALED, 3)] {
            let store = CarriedWalkStore::default();
            let rendered = rendered_metrics(|| {
                store.put(walk(1, 1));
                assert!(store.take::<Noop>(parent_hash, block_number).is_none());
            });

            assert!(
                rendered
                    .contains("base_builder_native_resting_walk_reset_total{reason=\"reorg\"} 1"),
                "{rendered}"
            );
        }
    }

    /// A walk carried for [`MAX_CARRIED_ITERATOR_BLOCKS`] blocks is reset for the next block,
    /// and one block earlier it is still carried.
    #[test]
    fn walk_is_reset_once_its_iterator_spans_the_block_limit() {
        let store = CarriedWalkStore::default();
        let last_carried = MAX_CARRIED_ITERATOR_BLOCKS - 1;
        store.put(walk(1, last_carried));
        assert!(store.take::<Noop>(SEALED, last_carried + 1).is_some());

        let rendered = rendered_metrics(|| {
            store.put(walk(1, MAX_CARRIED_ITERATOR_BLOCKS));
            assert!(store.take::<Noop>(SEALED, MAX_CARRIED_ITERATOR_BLOCKS + 1).is_none());
        });
        assert!(
            rendered
                .contains("base_builder_native_resting_walk_reset_total{reason=\"block_limit\"} 1"),
            "{rendered}"
        );
    }
}
