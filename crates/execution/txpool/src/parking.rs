//! Lane-aware parking over an existing best-transactions iterator.

use std::{
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{B256Map, HashMap, hash_map::Entry},
};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, EthPoolTransaction, PoolTransaction,
    TransactionOrdering, TransactionPool, ValidPoolTransaction, error::InvalidPoolTransactionError,
    noop::NoopTransactionPool,
};
use revm::{database::TransitionState, state::EvmState};

use crate::{BasePooledTx, BestTransactionPriority, DormancyBuild, UnifiedTipOrdering};

/// A sequential transaction lane whose members must execute in nonce order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BestTransactionLane {
    /// Transaction sender whose nonces define the lane.
    pub sender: Address,
    /// Nonce channel key, with zero representing the protocol nonce lane.
    pub nonce_key: U256,
}

/// Iteration-local state for a sequential transaction lane.
#[derive(Debug)]
pub enum BestTransactionLaneState<T>
where
    T: PoolTransaction,
{
    /// The lane has an unresolved head and buffers any descendants yielded by the source.
    Occupied(VecDeque<Arc<ValidPoolTransaction<T>>>),
    /// The lane was terminally invalidated and is excluded for the remainder of the iterator.
    Invalid,
}

impl BestTransactionLane {
    /// Returns the sequential lane for a transaction, or `None` for an independent nonce-free
    /// Base transaction.
    pub fn for_transaction<T>(transaction: &Arc<ValidPoolTransaction<T>>) -> Option<Self>
    where
        T: BasePooledTx,
    {
        let nonce_key = transaction.transaction.base_tx_nonce_channel_key();
        if nonce_key.is_none() && transaction.transaction.is_base_tx_nonce_free() {
            return None;
        }
        Some(Self { sender: transaction.sender(), nonce_key: nonce_key.unwrap_or_default() })
    }
}

/// Extra lifecycle operations required to temporarily park best transactions.
pub trait ParkableBestTransactions<T>:
    BestTransactions<Item = Arc<ValidPoolTransaction<T>>>
where
    T: PoolTransaction,
{
    /// Temporarily parks a transaction that was yielded by this iterator.
    fn park(&mut self, transaction: &Arc<ValidPoolTransaction<T>>);

    /// Makes a parked transaction eligible to compete by priority again.
    fn promote(&mut self, transaction_hash: TxHash) -> bool;

    /// Invalidates a parked transaction for the remainder of this iterator.
    fn discard_parked(
        &mut self,
        transaction_hash: TxHash,
        kind: InvalidPoolTransactionError,
    ) -> bool;

    /// Records that a yielded transaction committed and releases its lane successor.
    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<T>>);

    /// Offers the dormant transactions a committed transaction's `state` may satisfy, each
    /// followed by the rest of its lane. The default keeps no dormancy index and wakes nothing.
    fn wake_by_state(&mut self, _state: &EvmState) {}
}

/// A woken dormant transaction followed by the rest of its lane, in nonce order.
pub type WokenLane<T> = (Arc<ValidPoolTransaction<T>>, Vec<Arc<ValidPoolTransaction<T>>>);

/// Wakes the dormant transactions a commit that wrote the given state may satisfy.
pub type DormancyWake<T> = Box<dyn Fn(&EvmState) -> Vec<WokenLane<T>> + Send + Sync>;

/// Decides which validity candidates a parking iterator parks before yielding them.
///
/// The build loop parks a validity transaction once it finds one of its predicates unsatisfied,
/// and the pool re-yields that transaction in every later iterator until a state change wakes it.
/// A hold recognises such transactions at the source, so they are parked as if the build loop
/// had yielded and parked them, without the build loop seeing them: nothing behind them in
/// their lane is yielded and [`ParkableBestTransactions::promote`] returns them to priority
/// competition.
///
/// A pool asks the hold while it snapshots its stores about the candidates no lane or store
/// sequences behind another transaction, and leaves the held ones out of the iterator; see
/// [`ParkedBestTransactions::with_held_at_source`]. The iterator asks it about the candidates
/// that reach it.
///
/// Snapshot filtering invokes the hold only after releasing both lane-store read guards.
pub trait BestTransactionsHold<T: PoolTransaction>: Send + Sync {
    /// Returns whether `transaction` is parked instead of yielded. Called for candidates that
    /// carry validity predicates and are not blocked by their lane: when the pool is
    /// snapshotted or when the candidate reaches the iterator, and at both points for a
    /// candidate the snapshot asked about and did not hold.
    fn hold(&mut self, transaction: &ValidPoolTransaction<T>) -> bool;

    /// Records the time spent holding transactions back: by a pool snapshot from its first hold
    /// until it has filtered the independent heads, and by one `next` call from its first hold until it
    /// found a candidate, which includes advancing past further held transactions. Snapshots
    /// and calls that hold nothing record nothing.
    fn record_duration(&mut self, duration: Duration);

    /// Returns whether the build driving the iterator has stopped selecting transactions.
    ///
    /// A pool snapshot and one `next` call can each hold many transactions without returning
    /// to the build loop, so both check this every [`HOLD_INTERRUPT_INTERVAL`] holds and stop
    /// early once it is true. The default never interrupts.
    fn is_interrupted(&self) -> bool {
        false
    }
}

/// A transaction a pool snapshot held back with a [`BestTransactionsHold`] instead of offering
/// it to the iterator, identified without keeping a reference to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldBestTransaction {
    /// Hash of the held transaction.
    pub hash: TxHash,
    /// When the pool admitted the held transaction. A transaction re-admitted under the same
    /// hash, which can carry other validity predicates, was admitted at another instant.
    pub admitted_at: Instant,
}

/// Returns the transaction a pool holds under a hash.
pub type HeldTransactionLookup<T> =
    Box<dyn Fn(&TxHash) -> Option<Arc<ValidPoolTransaction<T>>> + Send + Sync>;

/// Number of transactions a pool snapshot or a parking iterator holds between checks of
/// [`BestTransactionsHold::is_interrupted`], so the check stays off the per-transaction path.
pub const HOLD_INTERRUPT_INTERVAL: u32 = 256;

/// A transaction pool that can create lane-aware parkable best iterators.
pub trait ParkableTransactionPool: TransactionPool
where
    Self::Transaction: BasePooledTx,
{
    /// Returns a parkable best iterator using the supplied fee attributes. For the build that
    /// owns the dormancy index, `dormancy`, it leaves dormant transactions out and offers them
    /// again when a commit wakes them.
    ///
    /// The iterator is a snapshot of every store taken when it is created:
    /// transactions added afterwards are not yielded, so a block build only
    /// sees transactions that arrived before it started. The exception is a
    /// lane a commit wakes from dormancy, which rejoins with its current run.
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
        hold: Option<Box<dyn BestTransactionsHold<Self::Transaction>>>,
        dormancy: Option<&DormancyBuild>,
    ) -> Box<dyn ParkableBestTransactions<Self::Transaction>>;

    /// Starts a build of block `block_number` on `parent` against the pool's dormancy index,
    /// after the build applied `pre_selection` writes such as system calls and deposits, and
    /// before it takes its snapshot. `None` when the pool keeps no index, which the default never
    /// does, or while another build owns it.
    fn begin_dormancy_build(
        &self,
        _parent: B256,
        _block_number: u64,
        _pre_selection: Option<&TransitionState>,
    ) -> Option<DormancyBuild> {
        None
    }
}

/// Lane-aware parking over an existing best-transactions iterator.
///
/// The inner iterator remains responsible for nonce contiguity and source ordering. This adapter
/// buffers descendants that the inner iterator unlocks while an earlier member of their lane is
/// parked or waiting for an execution outcome, parks the candidates a
/// [`BestTransactionsHold`] holds before they are yielded, and offers the dormant transactions a
/// commit wakes.
pub struct ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    inner: I,
    ordering: O,
    base_fee: u64,
    source_head: Option<(Arc<ValidPoolTransaction<T>>, Option<BestTransactionLane>)>,
    lanes: HashMap<BestTransactionLane, BestTransactionLaneState<T>>,
    parked: HashMap<TxHash, (Arc<ValidPoolTransaction<T>>, Option<BestTransactionLane>)>,
    ready: HashMap<TxHash, (Arc<ValidPoolTransaction<T>>, Option<BestTransactionLane>)>,
    ready_heap: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, TxHash)>,
    hold: Option<Box<dyn BestTransactionsHold<T>>>,
    /// The transactions the source's snapshot held back, parked without an entry in `parked`.
    /// Few are promoted in a build, so they stay in the list the snapshot produced until the
    /// first one is.
    held_at_source: Vec<HeldBestTransaction>,
    /// The admission instants of the transactions the source's snapshot held back that are
    /// still parked, by hash. Takes over from `held_at_source` when the first one is promoted.
    held_at_source_parked: Option<B256Map<Instant>>,
    /// Fetches a transaction the source's snapshot held back when it is promoted.
    held_at_source_lookup: Option<HeldTransactionLookup<T>>,
    wake: Option<DormancyWake<T>>,
}

impl<T, I, O> std::fmt::Debug for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedBestTransactions")
            .field("lanes", &self.lanes.len())
            .field("parked", &self.parked.len())
            .field("ready", &self.ready.len())
            .field("hold", &self.hold.is_some())
            .field(
                "held_at_source",
                &self
                    .held_at_source_parked
                    .as_ref()
                    .map_or(self.held_at_source.len(), B256Map::len),
            )
            .field("wake", &self.wake.is_some())
            .finish_non_exhaustive()
    }
}

impl<T, I, O> ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    /// Creates a lane-aware parking adapter.
    pub fn new(inner: I, ordering: O, base_fee: u64) -> Self {
        Self {
            inner,
            ordering,
            base_fee,
            source_head: None,
            lanes: HashMap::default(),
            parked: HashMap::default(),
            ready: HashMap::default(),
            ready_heap: BinaryHeap::new(),
            hold: None,
            held_at_source: Vec::new(),
            held_at_source_parked: None,
            held_at_source_lookup: None,
            wake: None,
        }
    }

    /// Parks the candidates `hold` holds before they are yielded.
    pub fn with_hold(mut self, hold: Option<Box<dyn BestTransactionsHold<T>>>) -> Self {
        self.hold = hold;
        self
    }

    /// Parks the transactions the source's snapshot held back with the same hold. No other
    /// pending transaction is sequenced behind one, so it is parked as a transaction this
    /// iterator's own hold held, without its lane being occupied.
    ///
    /// [`ParkableBestTransactions::promote`] fetches one with `lookup` and returns it to
    /// priority competition. One the pool no longer holds, or has since admitted again, stays
    /// out of the build, like any transaction that arrives after the snapshot.
    pub fn with_held_at_source(
        mut self,
        held: Vec<HeldBestTransaction>,
        lookup: HeldTransactionLookup<T>,
    ) -> Self {
        self.held_at_source = held;
        self.held_at_source_parked = None;
        self.held_at_source_lookup = Some(lookup);
        self
    }

    /// Takes the transaction the source's snapshot held back under `transaction_hash`.
    pub fn take_held_at_source(
        &mut self,
        transaction_hash: TxHash,
    ) -> Option<(Arc<ValidPoolTransaction<T>>, Option<BestTransactionLane>)> {
        if self.held_at_source_parked.is_none() && self.held_at_source.is_empty() {
            return None;
        }
        let parked = self.held_at_source_parked.get_or_insert_with(|| {
            std::mem::take(&mut self.held_at_source)
                .into_iter()
                .map(|held| (held.hash, held.admitted_at))
                .collect()
        });
        let admitted_at = parked.remove(&transaction_hash)?;
        let transaction = self.held_at_source_lookup.as_ref()?(&transaction_hash)
            .filter(|transaction| transaction.timestamp == admitted_at)?;
        let lane = BestTransactionLane::for_transaction(&transaction);
        Some((transaction, lane))
    }

    /// Offers the dormant transactions `wake` returns for each commit.
    pub fn with_dormancy_wake(mut self, wake: Option<DormancyWake<T>>) -> Self {
        self.wake = wake;
        self
    }

    /// Offers a transaction the source never yielded, followed in its lane by `run`, as if it had
    /// been parked and promoted. Skipped when the iterator already holds a member of its lane.
    pub fn inject(
        &mut self,
        transaction: Arc<ValidPoolTransaction<T>>,
        run: Vec<Arc<ValidPoolTransaction<T>>>,
    ) {
        let lane = BestTransactionLane::for_transaction(&transaction);
        if let Some(lane) = lane {
            match self.lanes.entry(lane) {
                Entry::Occupied(_) => return,
                Entry::Vacant(vacant) => {
                    vacant.insert(BestTransactionLaneState::Occupied(run.into()));
                }
            }
        }
        self.push_ready(transaction, lane);
    }

    /// Returns a complete priority key for a transaction.
    pub fn priority(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> BestTransactionPriority<O::PriorityValue> {
        BestTransactionPriority::new(&self.ordering, transaction, self.base_fee)
    }

    /// Adds a transaction and its sequential lane to the priority-ordered ready set.
    pub fn push_ready(
        &mut self,
        transaction: Arc<ValidPoolTransaction<T>>,
        lane: Option<BestTransactionLane>,
    ) {
        let hash = *transaction.hash();
        if self.ready.contains_key(&hash) {
            return;
        }
        let priority = self.priority(&transaction);
        self.ready.insert(hash, (transaction, lane));
        self.ready_heap.push((priority, hash));
    }

    /// Releases the next buffered transaction in a committed lane.
    pub fn release_lane(&mut self, lane: BestTransactionLane) {
        let next = match self.lanes.entry(lane) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                BestTransactionLaneState::Occupied(buffered) => {
                    let next = buffered.pop_front();
                    if next.is_none() {
                        entry.remove();
                    }
                    next
                }
                BestTransactionLaneState::Invalid => return,
            },
            Entry::Vacant(_) => return,
        };
        if let Some(next) = next {
            self.push_ready(next, Some(lane));
        }
    }

    /// Terminally invalidates a lane for the remainder of this iterator.
    ///
    /// Buffered descendants have already been consumed from `inner`. Invalidating the yielded lane
    /// head notifies `inner`, whose lane bookkeeping excludes its remaining descendants.
    ///
    /// Parked and ready members of the lane are not removed here: the build loop invalidates a
    /// lane on every rejection, and scanning both maps each time costs O(parked + ready) per
    /// rejection. [`BestTransactionLaneState::Invalid`] is terminal, so `promote`, `discard_parked`,
    /// [`Self::ready_priority`], and [`Self::pop_ready`] drop those members when they next touch
    /// them.
    pub fn invalidate_lane(&mut self, lane: BestTransactionLane) {
        self.lanes.insert(lane, BestTransactionLaneState::Invalid);
        if self.source_head.as_ref().is_some_and(|(_, head_lane)| *head_lane == Some(lane)) {
            self.source_head = None;
        }
    }

    /// Returns whether `lane` was terminally invalidated during this iteration.
    pub fn is_lane_invalid(&self, lane: Option<BestTransactionLane>) -> bool {
        lane.is_some_and(|lane| {
            matches!(self.lanes.get(&lane), Some(BestTransactionLaneState::Invalid))
        })
    }

    /// Pulls through blocked descendants and held candidates until the next source candidate is
    /// lane-eligible.
    ///
    /// Consuming occupied-lane descendants is the intentionally naive BASE-253 implementation:
    /// the source only exposes other lanes after these transactions are pulled and buffered. The
    /// `Invalid` arm is defensive because terminal invalidation is already forwarded to `inner`,
    /// whose [`BestTransactions`] contract excludes the invalid transaction's descendants.
    ///
    /// Every [`HOLD_INTERRUPT_INTERVAL`] holds it stops early, without a source head, once the
    /// hold reports its build interrupted. `next` then yields a promoted transaction or `None`,
    /// and the build loop observes the same interruption and stops.
    pub fn fill_source_head(&mut self) {
        let mut hold_start = None;
        let mut held = 0_u32;
        while self.source_head.is_none() {
            let Some(transaction) = self.inner.next() else {
                break;
            };
            let lane = BestTransactionLane::for_transaction(&transaction);
            // One lookup serves both the lane check and occupying the lane of a held head.
            let vacant_lane = match lane.map(|lane| self.lanes.entry(lane)) {
                Some(Entry::Occupied(mut entry)) => match entry.get_mut() {
                    BestTransactionLaneState::Invalid => continue,
                    BestTransactionLaneState::Occupied(buffered) => {
                        buffered.push_back(transaction);
                        continue;
                    }
                },
                Some(Entry::Vacant(vacant)) => Some(vacant),
                None => None,
            };
            if let Some(hold) = self.hold.as_mut()
                && !transaction.transaction.validity_conditions().is_empty()
                && hold.hold(&transaction)
            {
                // The clock starts at the first hold, so candidates that hold nothing pay no
                // timer read.
                hold_start.get_or_insert_with(Instant::now);
                // Parked as if yielded and parked: the lane is occupied so descendants are
                // buffered until the held head is promoted and committed.
                if let Some(vacant) = vacant_lane {
                    vacant.insert(BestTransactionLaneState::Occupied(VecDeque::new()));
                }
                self.parked.insert(*transaction.hash(), (transaction, lane));
                held += 1;
                if held.is_multiple_of(HOLD_INTERRUPT_INTERVAL) && hold.is_interrupted() {
                    break;
                }
                continue;
            }
            self.source_head = Some((transaction, lane));
        }
        if let Some(start) = hold_start
            && let Some(hold) = self.hold.as_mut()
        {
            hold.record_duration(start.elapsed());
        }
    }

    /// Removes stale and invalid-lane heap entries and returns the highest-priority ready key.
    pub fn ready_priority(&mut self) -> Option<&BestTransactionPriority<O::PriorityValue>> {
        while let Some(&(_, hash)) = self.ready_heap.peek() {
            let live = self.ready.get(&hash).map(|(_, lane)| !self.is_lane_invalid(*lane));
            if live == Some(true) {
                break;
            }
            if live == Some(false) {
                self.ready.remove(&hash);
            }
            self.ready_heap.pop();
        }
        self.ready_heap.peek().map(|(priority, _)| priority)
    }

    /// Pops the highest-priority ready transaction and its sequential lane, skipping stale heap
    /// entries and members of invalidated lanes.
    pub fn pop_ready(
        &mut self,
    ) -> Option<(Arc<ValidPoolTransaction<T>>, Option<BestTransactionLane>)> {
        loop {
            let (_, hash) = self.ready_heap.pop()?;
            if let Some(entry) = self.ready.remove(&hash)
                && !self.is_lane_invalid(entry.1)
            {
                return Some(entry);
            }
        }
    }

    /// Records a transaction as yielded and occupies its sequential lane.
    pub fn record_yielded(
        &mut self,
        transaction: Arc<ValidPoolTransaction<T>>,
        lane: Option<BestTransactionLane>,
    ) -> Arc<ValidPoolTransaction<T>> {
        if let Some(lane) = lane {
            self.lanes
                .entry(lane)
                .or_insert_with(|| BestTransactionLaneState::Occupied(VecDeque::new()));
        }
        transaction
    }
}

impl<T, I, O> Iterator for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.fill_source_head();

        let ready_priority = self.ready_priority().cloned();
        let source_priority = self.source_head.as_ref().map(|(source, _)| self.priority(source));
        let take_ready = match (source_priority, ready_priority) {
            (Some(source), Some(ready)) => ready >= source,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => return None,
        };

        let (transaction, lane) = if take_ready {
            self.pop_ready().expect("ready priority requires a ready transaction")
        } else {
            self.source_head.take().expect("source priority requires a source transaction")
        };
        Some(self.record_yielded(transaction, lane))
    }
}

impl<T, I, O> BestTransactions for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if let Some(lane) = BestTransactionLane::for_transaction(transaction) {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.inner.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.inner.set_skip_blobs(skip_blobs);
    }
}

impl<T, I, O> ParkableBestTransactions<T> for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn park(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let lane = BestTransactionLane::for_transaction(transaction);
        self.parked.insert(*transaction.hash(), (Arc::clone(transaction), lane));
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        let parked = self
            .parked
            .remove(&transaction_hash)
            .or_else(|| self.take_held_at_source(transaction_hash));
        let Some((transaction, lane)) = parked else {
            return false;
        };
        if self.is_lane_invalid(lane) {
            return false;
        }
        self.push_ready(transaction, lane);
        true
    }

    fn discard_parked(
        &mut self,
        transaction_hash: TxHash,
        kind: InvalidPoolTransactionError,
    ) -> bool {
        let parked = self
            .parked
            .remove(&transaction_hash)
            .or_else(|| self.take_held_at_source(transaction_hash));
        let Some((transaction, lane)) = parked else {
            return false;
        };
        if self.is_lane_invalid(lane) {
            return false;
        }
        if let Some(lane) = lane {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(&transaction, kind);
        true
    }

    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        if let Some(lane) = BestTransactionLane::for_transaction(transaction) {
            self.release_lane(lane);
        }
    }

    fn wake_by_state(&mut self, state: &EvmState) {
        let Some(wake) = &self.wake else { return };
        for (transaction, run) in wake(state) {
            self.inject(transaction, run);
        }
    }
}

/// A noop pool holds no transactions, so parking over its empty best iterator is vacuous.
impl<T> ParkableTransactionPool for NoopTransactionPool<T>
where
    T: BasePooledTx + EthPoolTransaction,
{
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
        _hold: Option<Box<dyn BestTransactionsHold<Self::Transaction>>>,
        _dormancy: Option<&DormancyBuild>,
    ) -> Box<dyn ParkableBestTransactions<Self::Transaction>> {
        Box::new(ParkedBestTransactions::new(
            std::iter::empty(),
            UnifiedTipOrdering::default(),
            attributes.basefee,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Instant};

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::Bytes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, TxBase, TxBaseConstants, TxBaseSigned,
    };
    use parking_lot::Mutex;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{TransactionOrigin, identifier::TransactionId};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction, ValidityOperator, ValidityPredicate};

    #[derive(Debug)]
    struct StaticBestTransactions {
        transactions: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    }

    impl StaticBestTransactions {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl Iterator for StaticBestTransactions {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl BestTransactions for StaticBestTransactions {
        fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
            let lane = BestTransactionLane::for_transaction(transaction);
            self.transactions.retain(|candidate| {
                BestTransactionLane::for_transaction(candidate) != lane || lane.is_none()
            });
        }

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    fn transaction(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce: u64,
        priority_fee: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        validity_transaction(signer, nonce_key, nonce, priority_fee, Vec::new())
    }

    fn validity_transaction(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce: u64,
        priority_fee: u128,
        predicates: Vec<ValidityPredicate>,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxBase {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: nonce,
            valid_after: 0,
            valid_before: u64::from(nonce_key == TxBaseConstants::NONCE_KEY_MAX),
            max_priority_fee_per_gas: priority_fee,
            max_fee_per_gas: priority_fee + 10,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed = TxBaseSigned::new(tx, Bytes::from(signature.as_bytes()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Base(signed);
        let encoded_length = pooled.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(pooled.into(), signer.address()),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), nonce),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    #[test]
    fn parked_protocol_parent_releases_buffered_descendant_after_commit() {
        let parent_signer = PrivateKeySigner::random();
        let trigger_signer = PrivateKeySigner::random();
        let low_signer = PrivateKeySigner::random();
        let parent = transaction(&parent_signer, U256::ZERO, 0, 100);
        let child = transaction(&parent_signer, U256::ZERO, 1, 90);
        let trigger = transaction(&trigger_signer, U256::ZERO, 0, 50);
        let low = transaction(&low_signer, U256::ZERO, 0, 1);
        let parent_hash = *parent.hash();

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&parent),
            Arc::clone(&child),
            Arc::clone(&trigger),
            Arc::clone(&low),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert_eq!(best.next().unwrap().hash(), trigger.hash());
        best.mark_committed(&trigger);
        assert!(best.promote(parent_hash));
        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.mark_committed(&parent);
        assert_eq!(best.next().unwrap().hash(), child.hash());
        best.mark_committed(&child);
        assert_eq!(best.next().unwrap().hash(), low.hash());
    }

    #[test]
    fn finite_channels_and_nonce_free_transactions_are_independent() {
        let signer = PrivateKeySigner::random();
        let channel_one = U256::from(1);
        let channel_two = U256::from(2);
        let parent = transaction(&signer, channel_one, 0, 100);
        let child = transaction(&signer, channel_one, 1, 90);
        let other_channel = transaction(&signer, channel_two, 0, 50);
        let nonce_free = transaction(&signer, TxBaseConstants::NONCE_KEY_MAX, 0, 40);
        let parent_hash = *parent.hash();

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&parent),
            Arc::clone(&child),
            Arc::clone(&other_channel),
            Arc::clone(&nonce_free),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert_eq!(best.next().unwrap().hash(), other_channel.hash());
        best.mark_committed(&other_channel);
        assert_eq!(best.next().unwrap().hash(), nonce_free.hash());
        best.mark_committed(&nonce_free);
        assert!(best.promote(parent_hash));
        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.mark_committed(&parent);
        assert_eq!(best.next().unwrap().hash(), child.hash());
    }

    #[test]
    fn discarding_parked_head_excludes_its_lane_for_the_iteration() {
        let parent_signer = PrivateKeySigner::random();
        let other_signer = PrivateKeySigner::random();
        let parent = transaction(&parent_signer, U256::ZERO, 0, 100);
        let child = transaction(&parent_signer, U256::ZERO, 1, 90);
        let other = transaction(&other_signer, U256::ZERO, 0, 1);
        let parent_hash = *parent.hash();
        let inner =
            StaticBestTransactions::new(vec![Arc::clone(&parent), child, Arc::clone(&other)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert!(best.discard_parked(parent_hash, InvalidPoolTransactionError::Underpriced));
        assert_eq!(best.next().unwrap().hash(), other.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn invalidating_lane_excludes_its_ready_transactions() {
        let signer = PrivateKeySigner::random();
        let transaction = transaction(&signer, U256::from(1), 0, 100);
        let lane = BestTransactionLane::for_transaction(&transaction).unwrap();
        let inner = StaticBestTransactions::new(Vec::new());
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        best.push_ready(transaction, Some(lane));
        best.invalidate_lane(lane);

        assert!(best.ready_priority().is_none());
        assert!(best.next().is_none());
    }

    #[test]
    fn invalidated_ready_transaction_is_skipped_in_priority_order() {
        let invalid_signer = PrivateKeySigner::random();
        let high_signer = PrivateKeySigner::random();
        let low_signer = PrivateKeySigner::random();
        let invalid = transaction(&invalid_signer, U256::ZERO, 0, 100);
        let high = transaction(&high_signer, U256::ZERO, 0, 90);
        let low = transaction(&low_signer, U256::ZERO, 0, 10);
        let invalid_lane = BestTransactionLane::for_transaction(&invalid).unwrap();
        let inner = StaticBestTransactions::new(Vec::new());
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        for transaction in [&invalid, &high, &low] {
            let lane = BestTransactionLane::for_transaction(transaction);
            best.push_ready(Arc::clone(transaction), lane);
        }
        best.invalidate_lane(invalid_lane);

        assert_eq!(best.next().unwrap().hash(), high.hash());
        assert_eq!(best.next().unwrap().hash(), low.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn discarding_parked_member_of_invalidated_lane_is_a_no_op() {
        let signer = PrivateKeySigner::random();
        let parked = transaction(&signer, U256::from(1), 0, 100);
        let lane = BestTransactionLane::for_transaction(&parked).unwrap();
        let inner = StaticBestTransactions::new(vec![Arc::clone(&parked)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        let yielded = best.next().unwrap();
        best.park(&yielded);
        best.invalidate_lane(lane);

        assert!(!best.discard_parked(*parked.hash(), InvalidPoolTransactionError::Underpriced));
        assert!(!best.promote(*parked.hash()));
    }

    #[test]
    fn invalidating_lane_discards_only_its_parked_transactions() {
        let invalid_signer = PrivateKeySigner::random();
        let other_signer = PrivateKeySigner::random();
        let invalid_parked = transaction(&invalid_signer, U256::from(1), 0, 100);
        let other_parked = transaction(&other_signer, U256::ZERO, 0, 90);
        let invalid_lane = BestTransactionLane::for_transaction(&invalid_parked).unwrap();
        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&invalid_parked),
            Arc::clone(&other_parked),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        let first = best.next().unwrap();
        best.park(&first);
        let second = best.next().unwrap();
        best.park(&second);
        best.invalidate_lane(invalid_lane);

        assert!(!best.promote(*invalid_parked.hash()));
        assert!(best.promote(*other_parked.hash()));
        assert_eq!(best.next().unwrap().hash(), other_parked.hash());
        assert!(best.next().is_none());
    }

    /// Holds every validity transaction whose hash is in `held`, recording each check.
    struct HashSetHold {
        held: Vec<TxHash>,
        checked: Vec<TxHash>,
        durations: usize,
    }

    impl BestTransactionsHold<BasePooledTransaction> for Arc<Mutex<HashSetHold>> {
        fn hold(&mut self, transaction: &ValidPoolTransaction<BasePooledTransaction>) -> bool {
            let mut hold = self.lock();
            hold.checked.push(*transaction.hash());
            hold.held.contains(transaction.hash())
        }

        fn record_duration(&mut self, _duration: Duration) {
            self.lock().durations += 1;
        }
    }

    fn balance_predicate() -> ValidityPredicate {
        ValidityPredicate::Balance {
            address: Address::repeat_byte(0xaa),
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::ONE,
        }
    }

    #[test]
    fn held_transaction_is_parked_with_its_lane_until_promoted() {
        let held_signer = PrivateKeySigner::random();
        let other_signer = PrivateKeySigner::random();
        let held =
            validity_transaction(&held_signer, U256::ZERO, 0, 100, vec![balance_predicate()]);
        let child = transaction(&held_signer, U256::ZERO, 1, 90);
        let other = transaction(&other_signer, U256::ZERO, 0, 50);
        let hold = Arc::new(Mutex::new(HashSetHold {
            held: vec![*held.hash()],
            checked: Vec::new(),
            durations: 0,
        }));
        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&held),
            Arc::clone(&child),
            Arc::clone(&other),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0)
            .with_hold(Some(Box::new(Arc::clone(&hold))));

        // The held head and its descendant are not yielded; the other lane is.
        assert_eq!(best.next().unwrap().hash(), other.hash());
        best.mark_committed(&other);
        assert!(best.next().is_none());
        assert_eq!(hold.lock().checked, vec![*held.hash()], "plain transactions are not checked");
        assert_eq!(hold.lock().durations, 1);

        // Promotion returns the held head, and committing it releases its descendant.
        assert!(best.promote(*held.hash()));
        assert_eq!(best.next().unwrap().hash(), held.hash());
        best.mark_committed(&held);
        assert_eq!(best.next().unwrap().hash(), child.hash());
        assert!(best.next().is_none());
    }

    fn held_at_source(
        transaction: &ValidPoolTransaction<BasePooledTransaction>,
    ) -> HeldBestTransaction {
        HeldBestTransaction { hash: *transaction.hash(), admitted_at: transaction.timestamp }
    }

    /// A lookup over the transactions a pool still holds.
    fn lookup_in(
        pool: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    ) -> HeldTransactionLookup<BasePooledTransaction> {
        Box::new(move |hash| pool.iter().find(|transaction| transaction.hash() == hash).cloned())
    }

    #[test]
    fn transaction_held_at_the_source_is_parked_until_promoted() {
        let held_signer = PrivateKeySigner::random();
        let other_signer = PrivateKeySigner::random();
        let held =
            validity_transaction(&held_signer, U256::ZERO, 0, 100, vec![balance_predicate()]);
        let other = transaction(&other_signer, U256::ZERO, 0, 50);
        let lowest = transaction(&PrivateKeySigner::random(), U256::ZERO, 0, 10);
        // The source's snapshot left the held transaction out, so the source yields the rest.
        let inner = StaticBestTransactions::new(vec![Arc::clone(&other), Arc::clone(&lowest)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0)
            .with_held_at_source(vec![held_at_source(&held)], lookup_in(vec![Arc::clone(&held)]));

        assert_eq!(best.next().unwrap().hash(), other.hash());
        best.mark_committed(&other);

        // Promotion returns it to priority competition, ahead of what the source still holds.
        assert!(!best.promote(*lowest.hash()), "only held transactions are parked");
        assert!(best.promote(*held.hash()));
        assert!(!best.promote(*held.hash()), "a promoted transaction is no longer parked");
        assert_eq!(best.next().unwrap().hash(), held.hash());
        best.mark_committed(&held);
        assert_eq!(best.next().unwrap().hash(), lowest.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn discarded_transaction_held_at_the_source_is_not_promoted() {
        let held = validity_transaction(
            &PrivateKeySigner::random(),
            U256::ZERO,
            0,
            100,
            vec![balance_predicate()],
        );
        let mut best = ParkedBestTransactions::new(
            StaticBestTransactions::new(Vec::new()),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_held_at_source(vec![held_at_source(&held)], lookup_in(vec![Arc::clone(&held)]));

        assert!(best.discard_parked(*held.hash(), InvalidPoolTransactionError::Underpriced));
        assert!(!best.promote(*held.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn transaction_held_at_the_source_that_left_the_pool_is_not_promoted() {
        let held = validity_transaction(
            &PrivateKeySigner::random(),
            U256::ZERO,
            0,
            100,
            vec![balance_predicate()],
        );
        let mut best = ParkedBestTransactions::new(
            StaticBestTransactions::new(Vec::new()),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_held_at_source(vec![held_at_source(&held)], lookup_in(Vec::new()));

        assert!(!best.promote(*held.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn transaction_readmitted_under_the_held_hash_is_not_promoted() {
        let signer = PrivateKeySigner::random();
        let held = validity_transaction(&signer, U256::ZERO, 0, 100, vec![balance_predicate()]);
        // The same signed transaction, admitted again later; its sidecar may differ.
        let readmitted = Arc::new(ValidPoolTransaction {
            transaction_id: held.transaction_id,
            transaction: held.transaction.clone(),
            propagate: true,
            timestamp: held.timestamp + Duration::from_millis(1),
            origin: TransactionOrigin::External,
            authority_ids: None,
        });
        assert_eq!(readmitted.hash(), held.hash());
        let mut best = ParkedBestTransactions::new(
            StaticBestTransactions::new(Vec::new()),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_held_at_source(vec![held_at_source(&held)], lookup_in(vec![readmitted]));

        assert!(!best.promote(*held.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn unheld_validity_transaction_is_yielded_without_recording_hold_work() {
        let signer = PrivateKeySigner::random();
        let candidate =
            validity_transaction(&signer, U256::ZERO, 0, 100, vec![balance_predicate()]);
        let hold = Arc::new(Mutex::new(HashSetHold {
            held: Vec::new(),
            checked: Vec::new(),
            durations: 0,
        }));
        let inner = StaticBestTransactions::new(vec![Arc::clone(&candidate)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0)
            .with_hold(Some(Box::new(Arc::clone(&hold))));

        assert_eq!(best.next().unwrap().hash(), candidate.hash());
        assert_eq!(hold.lock().checked, vec![*candidate.hash()]);
        assert_eq!(hold.lock().durations, 0);
    }

    /// Holds every validity transaction until `interrupted` is cleared, counting holds.
    struct InterruptibleHold {
        interrupted: bool,
        held: usize,
    }

    impl BestTransactionsHold<BasePooledTransaction> for Arc<Mutex<InterruptibleHold>> {
        fn hold(&mut self, _transaction: &ValidPoolTransaction<BasePooledTransaction>) -> bool {
            self.lock().held += 1;
            true
        }

        fn record_duration(&mut self, _duration: Duration) {}

        fn is_interrupted(&self) -> bool {
            self.lock().interrupted
        }
    }

    #[test]
    fn interrupted_hold_stops_advancing_past_held_transactions() {
        let held_count = 2 * HOLD_INTERRUPT_INTERVAL as usize + 1;
        let mut transactions = (0..held_count)
            .map(|_| {
                let signer = PrivateKeySigner::random();
                validity_transaction(&signer, U256::ZERO, 0, 100, vec![balance_predicate()])
            })
            .collect::<Vec<_>>();
        let plain = transaction(&PrivateKeySigner::random(), U256::ZERO, 0, 50);
        transactions.push(Arc::clone(&plain));
        let hold = Arc::new(Mutex::new(InterruptibleHold { interrupted: true, held: 0 }));
        let mut best = ParkedBestTransactions::new(
            StaticBestTransactions::new(transactions),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_hold(Some(Box::new(Arc::clone(&hold))));

        // An interrupted build gets control back after one check interval of holds.
        assert!(best.next().is_none());
        assert_eq!(hold.lock().held, HOLD_INTERRUPT_INTERVAL as usize);

        // Nothing is lost: an uninterrupted call resumes the walk where it stopped.
        hold.lock().interrupted = false;
        assert_eq!(best.next().unwrap().hash(), plain.hash());
        assert_eq!(hold.lock().held, held_count);
        assert_eq!(best.parked.len(), held_count);
    }
}
