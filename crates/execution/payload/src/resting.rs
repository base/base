//! Validity transactions resting under an unsatisfied predicate, carried across block builds.

use std::{
    collections::BTreeMap,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alloy_primitives::{B256, TxHash, map::B256Set};
use base_execution_txpool::{
    BasePooledTx, BestTransactionsHold, PredicateContext, SelectionView, ValidityConditions,
    ValidityPredicate,
};
use parking_lot::Mutex;
use reth_tasks::Runtime;
use reth_transaction_pool::ValidPoolTransaction;
use revm::{database::TransitionState, state::EvmState};
use tracing::debug_span;

use crate::{ParkedPredicateIndex, ValidityMetrics, ValidityPredicateKey};

/// Blocks a member stays in the set after the last build that yielded it.
///
/// Every state change wakes members whether or not the pool still yields them, so keeping a member
/// longer only costs memory. A member that left the pool is dropped when the block this many
/// blocks after the last build that yielded it finishes, two seconds at the 200ms block time Denim
/// activates. The sweep that drops it only visits the members filed that many blocks ago, and runs
/// off the build path; see [`RestingPredicateStore::finish`].
pub const RESTING_UNSEEN_RETAINED_BLOCKS: u64 = 10;

/// `filed_block` of a member dormant in the selection view. It is listed in no generation, so the
/// unseen sweep never visits it; the pool reports its removal instead.
const DORMANT: u64 = u64::MAX;

/// Mode for holding back validity transactions that rest under an unchanged predicate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
pub enum RestingPredicateMode {
    /// Resting transactions are not tracked.
    #[default]
    Off,
    /// Resting transactions are tracked and still evaluated. A resting transaction whose
    /// predicates match is counted as a mismatch, which would be a missed inclusion under
    /// enforcement.
    Shadow,
    /// Resting transactions are parked without being evaluated until a state change that can
    /// satisfy their predicate wakes them.
    Enforce,
}

impl RestingPredicateMode {
    /// Returns whether resting transactions are tracked.
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Shadow | Self::Enforce)
    }

    /// Returns whether resting transactions are held back.
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Blocks in which a resting transaction was last evaluated and last yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestingMember {
    /// Block whose build last found the recorded predicate unsatisfied.
    pub rested_block: u64,
    /// Block whose build last yielded the transaction.
    pub seen_block: u64,
    /// Block whose generation lists the member, so the sweep visits it once that block is
    /// [`RESTING_UNSEEN_RETAINED_BLOCKS`] old.
    filed_block: u64,
    /// First block at which the transaction's whole validity conditions are expired, as the last
    /// hold saw them.
    expires_at: Option<u64>,
    /// The deadline the member is listed under while dormant.
    listed_deadline: Option<u64>,
}

/// What [`RestingPredicates::begin_block`] did to the set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestingCarryOver {
    /// The set was emptied because the new block does not build on the last block it followed.
    pub reset: bool,
    /// Members released because a pre-block write changed the state their predicate reads.
    pub released_by_state: usize,
    /// Members released because their predicate reads the block number.
    pub released_by_block_number: usize,
    /// Members carried into the new block.
    pub carried: usize,
}

/// Validity transactions whose recorded predicate is unsatisfied at the current build position.
///
/// A member's predicate stays false until state it reads changes. Builders report every state
/// change: commits in the selection loop through [`Self::woken_by_state`], and the parent state
/// plus pre-block writes through [`Self::begin_block`]. Skipping a member is therefore the same as
/// evaluating it and parking it again. The set may over-wake but must never miss a wakeup.
///
/// Members are only valid against the state they were checked on. The set records the block whose
/// final state it last followed, and empties itself when the next block builds on anything else,
/// such as a block built by another sequencer or a different payload version for the same height.
#[derive(Debug)]
pub struct RestingPredicates {
    index: ParkedPredicateIndex<RestingMember>,
    ordered_threshold: usize,
    block_number: u64,
    /// Hash of the block whose final state the members were last checked against. `None` while a
    /// block is being built, so a build that does not finish cannot hand its members on.
    tip: Option<B256>,
    /// Members by the block they are filed under. A member is filed under the block that rested
    /// it, and re-filed under its latest seen block when the sweep finds it was seen since, so the
    /// sweep only visits members old enough to drop. Entries for members that were removed or
    /// re-filed since are stale and skipped.
    generations: BTreeMap<u64, Vec<TxHash>>,
    /// Dormant members by the block at which their whole validity conditions expire, so block start
    /// wakes them for the builder to classify without visiting other dormant members. Holds exactly
    /// the dormant members with a deadline: leaving dormancy or the set drops the entry.
    deadlines: BTreeMap<u64, B256Set>,
    /// The pool iterators' persistent selection state for this lineage, carried, reset and
    /// dropped with the members.
    view: SelectionView,
}

impl RestingPredicates {
    /// Creates an empty set whose predicate buckets convert at `ordered_threshold` entries.
    pub fn new(ordered_threshold: usize) -> Self {
        Self {
            index: ParkedPredicateIndex::new(ordered_threshold),
            ordered_threshold,
            block_number: 0,
            tip: None,
            generations: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            view: SelectionView::default(),
        }
    }

    /// Starts building `block_number` on `parent_hash`.
    ///
    /// `pre_block_changes` must hold every write made on top of the parent state before the
    /// selection loop starts, such as system calls and deposits. Members are kept only when the
    /// parent is the block this set last finished and those writes are known; otherwise the set
    /// is emptied. Members blocked by a block-number predicate are always released so the new
    /// block re-evaluates them.
    pub fn begin_block(
        &mut self,
        parent_hash: B256,
        block_number: u64,
        pre_block_changes: Option<&TransitionState>,
    ) -> RestingCarryOver {
        let mut carry_over = RestingCarryOver::default();
        let follows_parent = self.tip.take() == Some(parent_hash);
        match pre_block_changes.filter(|_| follows_parent) {
            None => {
                carry_over.reset = !self.index.is_empty();
                self.index = ParkedPredicateIndex::new(self.ordered_threshold);
                self.generations.clear();
                self.deadlines.clear();
                self.view = SelectionView::default();
            }
            Some(changes) => {
                for hash in self.index.affected_by_transitions(changes).affected_transactions {
                    self.view.wake(hash);
                    carry_over.released_by_state += usize::from(self.remove_member(hash));
                }
                for hash in self.index.transactions_blocked_by(ValidityPredicateKey::BlockNumber) {
                    self.view.wake(hash);
                    carry_over.released_by_block_number += usize::from(self.remove_member(hash));
                }
                self.wake_expired_dormant(block_number);
            }
        }
        self.block_number = block_number;
        carry_over.carried = self.index.len();
        carry_over
    }

    /// Records that the block started by [`Self::begin_block`] was finished as `block_hash`, with
    /// every commit reported to the set.
    pub const fn end_block(&mut self, block_hash: B256) {
        self.tip = Some(block_hash);
    }

    /// Returns whether finishing the block being built is due an unseen sweep, because some
    /// members are filed under a block at least [`RESTING_UNSEEN_RETAINED_BLOCKS`] old.
    pub fn sweep_due(&self) -> bool {
        self.due_generation().is_some_and(|due| {
            self.generations.first_key_value().is_some_and(|(filed, _)| *filed <= due)
        })
    }

    /// Latest block whose generation is old enough to sweep.
    const fn due_generation(&self) -> Option<u64> {
        self.block_number.checked_sub(RESTING_UNSEEN_RETAINED_BLOCKS)
    }

    /// Drops members no build yielded in the last [`RESTING_UNSEEN_RETAINED_BLOCKS`] blocks and
    /// returns how many were dropped.
    ///
    /// Only visits members filed under a block that is old enough: those not seen since are
    /// dropped and the rest are re-filed under the block that last saw them. Run every block, it
    /// costs about the members rested or re-filed in one block rather than the whole set.
    /// Builders run it through [`RestingPredicateStore::finish`], off the path between the
    /// forkchoice update and flashblock scheduling.
    pub fn sweep_unseen(&mut self) -> usize {
        let Some(due) = self.due_generation() else { return 0 };
        let span = debug_span!(
            target: "payload_builder",
            "resting_unseen_sweep",
            block_number = self.block_number,
            visited = tracing::field::Empty,
            released = tracing::field::Empty,
        );
        let _entered = span.enter();
        let start = Instant::now();
        let (mut visited, mut released) = (0, 0);
        while let Some(generation) = self.generations.first_entry() {
            if *generation.key() > due {
                break;
            }
            let (filed, hashes) = generation.remove_entry();
            visited += hashes.len();
            for hash in hashes {
                let Some(member) = self.index.transaction_mut(hash) else { continue };
                if member.filed_block != filed {
                    continue;
                }
                if member.seen_block > due {
                    member.filed_block = member.seen_block;
                    self.generations.entry(member.seen_block).or_default().push(hash);
                } else {
                    self.remove_member(hash);
                    released += 1;
                }
            }
        }
        ValidityMetrics::record_resting_unseen_sweep(released, start.elapsed());
        span.record("visited", visited);
        span.record("released", released);
        released
    }

    /// Records that `predicate` was unsatisfied for `transaction_hash` at the current build
    /// position.
    ///
    /// Flashblock-index predicates are not recorded because the index changes without any state
    /// change.
    pub fn rest(&mut self, transaction_hash: TxHash, predicate: ValidityPredicate) {
        if matches!(predicate, ValidityPredicate::FlashblockIndex { .. }) {
            return;
        }
        // A member resting again in the block that filed it is already listed in that block's
        // generation; listing it twice would only give the sweep a stale entry to skip.
        let previous = self.index.entry(transaction_hash).map(|(member, _)| *member);
        let filed = previous.is_some_and(|member| member.filed_block == self.block_number);
        if let Some(deadline) = previous.and_then(|member| member.listed_deadline) {
            self.unlist_deadline(transaction_hash, deadline);
        }
        let member = RestingMember {
            rested_block: self.block_number,
            seen_block: self.block_number,
            filed_block: self.block_number,
            expires_at: None,
            listed_deadline: None,
        };
        self.index.park(transaction_hash, member, predicate);
        if !filed {
            self.generations.entry(self.block_number).or_default().push(transaction_hash);
        }
    }

    /// Removes a member, returning whether it was present. A dormant root is woken, so the next
    /// pool iterator offers it again.
    pub fn remove(&mut self, transaction_hash: TxHash) -> bool {
        let removed = !self.index.is_empty() && self.remove_member(transaction_hash);
        // Every dormant root is a member, so only a removed member can be dormant.
        if removed {
            self.view.wake(transaction_hash);
        }
        removed
    }

    /// Applies what pool iterator creation recorded on the view: dormant members leave the unseen
    /// sweep, woken ones rejoin it as seen in this block, and members the pool removed or replaced
    /// leave the set. Returns how many dormant roots creation skipped.
    ///
    /// A root can be woken and held again by the same creation; holds come after wakes, so its
    /// final state is dormant.
    fn apply_selection_changes(&mut self) -> u64 {
        let changes = self.view.take_changes();
        for hash in changes.removed {
            self.remove_member(hash);
        }
        for hash in changes.woken {
            self.rejoin_unseen_sweep(hash, self.block_number);
        }
        for hash in changes.dormant {
            let Some(member) = self.index.transaction_mut(hash) else { continue };
            member.filed_block = DORMANT;
            let (listed, expires_at) = (member.listed_deadline, member.expires_at);
            member.listed_deadline = expires_at;
            if listed != expires_at {
                if let Some(deadline) = listed {
                    self.unlist_deadline(hash, deadline);
                }
                if let Some(deadline) = expires_at {
                    self.deadlines.entry(deadline).or_default().insert(hash);
                }
            }
        }
        changes.skipped
    }

    /// Removes a member and its deadline entry, returning whether it was present.
    fn remove_member(&mut self, hash: TxHash) -> bool {
        let Some(member) = self.index.remove(hash) else { return false };
        if let Some(deadline) = member.listed_deadline {
            self.unlist_deadline(hash, deadline);
        }
        true
    }

    /// Drops `hash` from the deadline bucket it is listed in.
    fn unlist_deadline(&mut self, hash: TxHash, deadline: u64) {
        if let Some(bucket) = self.deadlines.get_mut(&deadline) {
            bucket.remove(&hash);
            if bucket.is_empty() {
                self.deadlines.remove(&deadline);
            }
        }
    }

    /// Lists a dormant member in `block`'s generation again, as seen in it, so the unseen sweep
    /// covers it once more.
    fn rejoin_unseen_sweep(&mut self, hash: TxHash, block: u64) {
        let Some(member) = self.index.transaction_mut(hash) else { return };
        if member.filed_block != DORMANT {
            return;
        }
        member.seen_block = block;
        member.filed_block = block;
        let listed = member.listed_deadline.take();
        self.generations.entry(block).or_default().push(hash);
        if let Some(deadline) = listed {
            self.unlist_deadline(hash, deadline);
        }
    }

    /// Wakes the dormant members whose whole validity conditions are expired at `block_number`, so
    /// iterator creation asks about them and the builder classifies them expired, as for a root
    /// the hold was asked about. Visits only the members due.
    fn wake_expired_dormant(&mut self, block_number: u64) {
        while let Some(due) = self.deadlines.first_entry()
            && *due.key() <= block_number
        {
            let (deadline, hashes) = due.remove_entry();
            for hash in hashes {
                let Some(member) = self.index.transaction_mut(hash) else { continue };
                if member.listed_deadline != Some(deadline) {
                    continue;
                }
                member.listed_deadline = None;
                self.view.wake(hash);
                self.rejoin_unseen_sweep(hash, block_number);
            }
        }
    }

    /// Returns the member for a transaction with `conditions`.
    ///
    /// A hash re-added to the pool with a batch that no longer contains the recorded predicate is
    /// not a member, because sidecar predicates are not covered by the hash. Neither is a
    /// transaction whose batch expired, so the builder evaluates and drops it.
    pub fn member(
        &self,
        transaction_hash: TxHash,
        conditions: &ValidityConditions,
    ) -> Option<RestingMember> {
        if self.index.is_empty() {
            return None;
        }
        let (member, predicate) = self.index.entry(transaction_hash)?;
        Self::applies(*member, predicate, conditions, self.block_number).then_some(*member)
    }

    /// Holds back a member of the set: records that the current build yielded it and returns
    /// whether it was carried over from an earlier block. Uses one lookup, because builders call
    /// it for every validity candidate.
    pub fn hold(
        &mut self,
        transaction_hash: TxHash,
        conditions: &ValidityConditions,
    ) -> Option<bool> {
        if self.index.is_empty() {
            return None;
        }
        let block_number = self.block_number;
        let (member, predicate) = self.index.entry_mut(transaction_hash)?;
        if !Self::applies(*member, predicate, conditions, block_number) {
            return None;
        }
        let carried = member.rested_block < block_number;
        member.expires_at = conditions.block().expiry_bound().and_then(|max| max.checked_add(1));
        // Members that rested in this block were already seen in it.
        if carried {
            member.seen_block = block_number;
        }
        Some(carried)
    }

    /// Whether `member`, resting under `predicate`, applies to a transaction with `conditions`.
    ///
    /// Only carried members are checked for expiry: one that rested in the block being built was
    /// evaluated, and found unexpired, at the same block number.
    fn applies(
        member: RestingMember,
        predicate: &ValidityPredicate,
        conditions: &ValidityConditions,
        block_number: u64,
    ) -> bool {
        conditions.contains(predicate)
            && (member.rested_block == block_number
                || !conditions.is_expired(&PredicateContext { block_number, flashblock_index: 0 }))
    }

    /// Returns members whose predicate may have changed in `state`, one committed transaction's
    /// changes. Callers remove them with [`Self::remove`] once the commit is applied.
    pub fn woken_by_state(&self, state: &EvmState) -> Vec<TxHash> {
        if self.index.is_empty() {
            return Vec::new();
        }
        self.index.affected_by_state(state).affected_transactions
    }

    /// Returns whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Returns the number of members.
    pub fn len(&self) -> usize {
        self.index.len()
    }
}

/// A finished block's [`RestingPredicates`], possibly still being swept in the background.
#[derive(Debug)]
pub struct FinishingRestingPredicates(Finishing);

#[derive(Debug)]
enum Finishing {
    Ready(Box<RestingPredicates>),
    Sweeping(mpsc::Receiver<RestingPredicates>),
}

impl FinishingRestingPredicates {
    /// Returns the set, waiting up to `limit` for an unfinished sweep.
    ///
    /// Returns `None` when the sweep does not finish in time or its task was dropped without
    /// returning the set; the caller then starts with an empty set. Time spent waiting is recorded,
    /// because it delays the build that takes the set.
    pub fn wait(self, limit: Duration) -> Option<RestingPredicates> {
        match self.0 {
            Finishing::Ready(resting) => Some(*resting),
            Finishing::Sweeping(receiver) => match receiver.try_recv() {
                Ok(resting) => {
                    ValidityMetrics::record_resting_sweep_wait(Duration::ZERO);
                    Some(resting)
                }
                Err(mpsc::TryRecvError::Empty) => {
                    let start = Instant::now();
                    let resting = receiver.recv_timeout(limit);
                    ValidityMetrics::record_resting_sweep_wait(start.elapsed());
                    match resting {
                        Ok(resting) => Some(resting),
                        // The sweep finishes on its worker and drops the set there.
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            ValidityMetrics::record_resting_sweep_wait_timeout();
                            None
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => None,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => None,
            },
        }
    }
}

/// Work spent holding back resting transactions since the stats were last taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestingStats {
    /// Resting transactions parked without being yielded to the build loop.
    pub parked: u64,
    /// Of [`Self::parked`], transactions whose predicate was found unsatisfied in an earlier block.
    pub carried_parked: u64,
    /// Time the pool iterators spent advancing past held transactions, from the first hold of
    /// each `next` call until it found a candidate.
    pub duration: Duration,
    /// Dormant resting roots the pool iterators skipped at creation without asking the hold.
    /// They are not counted in [`Self::parked`].
    pub dormant_skipped: u64,
}

/// Reports whether the build driving a pool iterator has stopped selecting transactions.
pub type HoldInterrupt = Arc<dyn Fn() -> bool + Send + Sync>;

/// A resting set shared between one build and the pool iterators it drives.
///
/// The build loop populates the set through [`Self::rest`] and reports commits through
/// [`Self::wake_by_state`]. Each pool iterator the build creates carries a [`Self::iterator_hold`]
/// that parks members at the source, so the build loop never sees a transaction whose recorded
/// predicate is still unsatisfied. The iterator and the build loop run on the build thread, so
/// the lock is uncontended; it exists because the iterator is a boxed trait object the build loop
/// does not own.
#[derive(Clone)]
pub struct SharedRestingPredicates {
    inner: Arc<Mutex<SharedRestingState>>,
    /// Interrupts the walks of the iterators this handle's holds park members in.
    interrupt: Option<HoldInterrupt>,
}

impl std::fmt::Debug for SharedRestingPredicates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRestingPredicates")
            .field("inner", &self.inner)
            .field("interrupt", &self.interrupt.is_some())
            .finish()
    }
}

#[derive(Debug)]
struct SharedRestingState {
    predicates: RestingPredicates,
    /// Members parked by the current iterator's hold. The build loop promotes these when a commit
    /// wakes them; members the build loop parked itself are woken by its own predicate index.
    held: B256Set,
    stats: RestingStats,
}

impl SharedRestingPredicates {
    /// Shares `predicates` with the pool iterators of one build.
    pub fn new(predicates: RestingPredicates) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedRestingState {
                predicates,
                held: B256Set::default(),
                stats: RestingStats::default(),
            })),
            interrupt: None,
        }
    }

    /// Interrupts the holds this handle creates once `interrupt` returns true, so a pool iterator
    /// stops advancing past held members when its build is cancelled. Clones share the resting
    /// set, so a builder can give each iterator a handle that observes that iterator's build.
    #[must_use]
    pub fn with_interrupt(mut self, interrupt: HoldInterrupt) -> Self {
        self.interrupt = Some(interrupt);
        self
    }

    /// Returns a hold for a new pool iterator. Members the previous iterator held are forgotten,
    /// because they were parked in that iterator.
    pub fn iterator_hold<T: BasePooledTx>(&self) -> Box<dyn BestTransactionsHold<T>> {
        self.inner.lock().held.clear();
        Box::new(RestingHold { shared: self.clone() })
    }

    /// Records that `predicate` was unsatisfied for `transaction_hash` at the current build
    /// position.
    pub fn rest(&self, transaction_hash: TxHash, predicate: ValidityPredicate) {
        self.inner.lock().predicates.rest(transaction_hash, predicate);
    }

    /// Removes a member, returning whether it was present.
    pub fn remove(&self, transaction_hash: TxHash) -> bool {
        self.inner.lock().predicates.remove(transaction_hash)
    }

    /// Returns the member for a transaction with `conditions`.
    pub fn member(
        &self,
        transaction_hash: TxHash,
        conditions: &ValidityConditions,
    ) -> Option<RestingMember> {
        self.inner.lock().predicates.member(transaction_hash, conditions)
    }

    /// Removes the members whose predicate may have changed in `state`, one committed
    /// transaction's changes, and returns those the current iterator held, at creation or as
    /// dormant roots it skipped, so the caller can promote them.
    pub fn wake_by_state(&self, state: &EvmState) -> Vec<TxHash> {
        let mut inner = self.inner.lock();
        if inner.predicates.is_empty() {
            return Vec::new();
        }
        let mut held = Vec::new();
        for transaction_hash in inner.predicates.woken_by_state(state) {
            let dormant = inner.predicates.view.wake(transaction_hash);
            inner.predicates.remove(transaction_hash);
            if inner.held.remove(&transaction_hash) || dormant {
                held.push(transaction_hash);
            }
        }
        held
    }

    /// Returns and resets the work spent holding back resting transactions.
    pub fn take_stats(&self) -> RestingStats {
        std::mem::take(&mut self.inner.lock().stats)
    }

    /// Takes the resting set, leaving an empty one, so it can be carried into the next block.
    pub fn take_predicates(&self) -> RestingPredicates {
        let mut inner = self.inner.lock();
        let empty = RestingPredicates::new(inner.predicates.ordered_threshold);
        std::mem::replace(&mut inner.predicates, empty)
    }

    /// Returns the number of members.
    pub fn len(&self) -> usize {
        self.inner.lock().predicates.len()
    }

    /// Returns whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().predicates.is_empty()
    }
}

/// Parks members of a [`SharedRestingPredicates`] inside a pool iterator.
#[derive(Debug)]
struct RestingHold {
    shared: SharedRestingPredicates,
}

impl<T: BasePooledTx> BestTransactionsHold<T> for RestingHold {
    fn hold(&mut self, transaction: &ValidPoolTransaction<T>) -> bool {
        let mut inner = self.shared.inner.lock();
        let Some(carried) = inner
            .predicates
            .hold(*transaction.hash(), transaction.transaction.validity_conditions())
        else {
            return false;
        };
        inner.held.insert(*transaction.hash());
        inner.stats.parked += 1;
        inner.stats.carried_parked += u64::from(carried);
        true
    }

    fn has_candidates(&self) -> bool {
        !self.shared.is_empty()
    }

    fn record_duration(&mut self, duration: Duration) {
        self.shared.inner.lock().stats.duration += duration;
    }

    fn is_interrupted(&self) -> bool {
        self.shared.interrupt.as_ref().is_some_and(|interrupt| interrupt())
    }

    fn take_selection_view(&mut self) -> Option<SelectionView> {
        Some(std::mem::take(&mut self.shared.inner.lock().predicates.view))
    }

    fn restore_selection_view(&mut self, view: SelectionView) {
        let mut inner = self.shared.inner.lock();
        inner.predicates.view = view;
        inner.stats.dormant_skipped += inner.predicates.apply_selection_changes();
    }
}

/// Holds one builder's [`RestingPredicates`] between payload jobs.
///
/// A job takes the set when it starts and puts it back once it finishes a block. A job that fails
/// or is abandoned never puts it back, so the next job starts empty. Concurrent jobs each see at
/// most one of them holding the set.
#[derive(Debug, Clone, Default)]
pub struct RestingPredicateStore {
    members: Arc<Mutex<Option<(FinishingRestingPredicates, B256)>>>,
    /// Runs end-of-block sweeps in the background. Without one, sweeps run inline in
    /// [`Self::finish`].
    executor: Option<Runtime>,
}

impl RestingPredicateStore {
    /// Name of the task-executor worker that runs end-of-block sweeps.
    const SWEEP_WORKER: &'static str = "resting-sweep";

    /// Creates an empty store whose end-of-block sweeps run on `executor`.
    pub fn new(executor: Runtime) -> Self {
        Self { members: Arc::default(), executor: Some(executor) }
    }

    /// Takes the stored set, or creates an empty one with `ordered_threshold`.
    ///
    /// Waits up to `sweep_wait_limit` for a sweep the previous job started. A sweep still running
    /// after that is abandoned and the set returned empty: the build evaluates every validity
    /// transaction again, as after a reset, and the sweep drops the old set on its worker.
    pub fn take(&self, ordered_threshold: usize, sweep_wait_limit: Duration) -> RestingPredicates {
        let stored = self.members.lock().take();
        stored
            .and_then(|(finishing, block_hash)| {
                let mut resting = finishing.wait(sweep_wait_limit)?;
                resting.end_block(block_hash);
                Some(resting)
            })
            .unwrap_or_else(|| RestingPredicates::new(ordered_threshold))
    }

    /// Finishes the selection loop of the block being built.
    ///
    /// When an unseen sweep is due it runs on the task executor, so it overlaps the state root and
    /// the leeway before the next build instead of delaying that build's flashblocks. Hand the
    /// result to [`Self::put`] once the block is sealed.
    pub fn finish(&self, mut resting: RestingPredicates) -> FinishingRestingPredicates {
        if resting.is_empty() {
            // Every listed member was removed, so every generation entry is stale.
            resting.generations.clear();
            resting.deadlines.clear();
            return FinishingRestingPredicates(Finishing::Ready(Box::new(resting)));
        }
        if !resting.sweep_due() {
            return FinishingRestingPredicates(Finishing::Ready(Box::new(resting)));
        }
        let Some(executor) = &self.executor else {
            resting.sweep_unseen();
            return FinishingRestingPredicates(Finishing::Ready(Box::new(resting)));
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        // A persistent named worker, like `Runtime::spawn_drop`, avoids creating a thread per
        // sweep. The set comes back over a std channel because `take` may wait for it from inside
        // a payload job's `block_on`, where blocking on a tokio channel panics.
        let _ = executor.spawn_blocking_named(Self::SWEEP_WORKER, move || {
            resting.sweep_unseen();
            // The receiver is gone if the block failed to seal; the set is dropped with it.
            let _ = sender.send(resting);
        });
        FinishingRestingPredicates(Finishing::Sweeping(receiver))
    }

    /// Stores the set of the block finished as `block_hash` for the next job.
    pub fn put(&self, resting: FinishingRestingPredicates, block_hash: B256) {
        *self.members.lock() = Some((resting, block_hash));
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_execution_txpool::{BasePooledTransaction, SelectionRoot, ValidityOperator};
    use revm::{
        database::{AccountStatus, TransitionAccount, TransitionState, states::StorageSlot},
        state::{Account, AccountInfo},
    };

    use super::*;

    const WATCHED: Address = Address::repeat_byte(0xaa);
    const UNRELATED: Address = Address::repeat_byte(0xbb);
    const PARENT: B256 = B256::repeat_byte(0x01);
    /// Long enough for any test sweep to finish.
    const WAIT: Duration = Duration::from_secs(10);
    const BUILT: B256 = B256::repeat_byte(0x02);

    fn balance_at_least(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn storage_equals(address: Address, slot: u64, value: u64) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot: U256::from(slot),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(value),
        }
    }

    fn conditions(predicates: &[ValidityPredicate]) -> ValidityConditions {
        ValidityConditions::new(predicates.to_vec())
    }

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    fn balance_transition(address: Address, old: u64, new: u64) -> TransitionState {
        let account =
            |balance: u64| AccountInfo { balance: U256::from(balance), ..Default::default() };
        TransitionState::single(
            address,
            TransitionAccount {
                info: Some(account(new)),
                status: AccountStatus::Changed,
                previous_info: Some(account(old)),
                previous_status: AccountStatus::Loaded,
                storage: Default::default(),
                storage_was_destroyed: false,
            },
        )
    }

    fn storage_transition(address: Address, slot: u64, old: u64, new: u64) -> TransitionState {
        let mut storage = alloy_primitives::map::HashMap::default();
        storage
            .insert(U256::from(slot), StorageSlot::new_changed(U256::from(old), U256::from(new)));
        TransitionState::single(
            address,
            TransitionAccount {
                info: Some(AccountInfo::default()),
                status: AccountStatus::Changed,
                previous_info: Some(AccountInfo::default()),
                previous_status: AccountStatus::Loaded,
                storage,
                storage_was_destroyed: false,
            },
        )
    }

    /// A set that rested `predicates` (one transaction each) in block 1 and finished it as
    /// [`PARENT`].
    fn rested_in_parent(predicates: &[ValidityPredicate]) -> RestingPredicates {
        let mut resting = RestingPredicates::new(32);
        resting.begin_block(B256::ZERO, 1, Some(&TransitionState::default()));
        for (index, predicate) in predicates.iter().enumerate() {
            resting.rest(hash(index as u8), predicate.clone());
        }
        resting.end_block(PARENT);
        resting
    }

    #[test]
    fn an_iterator_hold_observes_rests_added_and_removed_after_creation() {
        let shared = SharedRestingPredicates::new(RestingPredicates::new(32));
        let hold = shared.iterator_hold::<BasePooledTransaction>();
        assert!(!hold.has_candidates());

        shared.rest(hash(0), balance_at_least(WATCHED, 1));
        assert!(hold.has_candidates());
        assert!(shared.remove(hash(0)));
        assert!(!hold.has_candidates());
    }

    #[test]
    fn members_carry_into_a_block_built_on_the_finished_block() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = rested_in_parent(std::slice::from_ref(&predicate));

        let carry_over = resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        assert_eq!(carry_over, RestingCarryOver { carried: 1, ..Default::default() });
        assert_eq!(resting.hold(hash(0), &conditions(&[predicate])), Some(true));
    }

    #[test]
    fn set_is_emptied_when_the_parent_is_not_the_finished_block() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = rested_in_parent(std::slice::from_ref(&predicate));

        let carry_over = resting.begin_block(BUILT, 2, Some(&TransitionState::default()));

        assert!(carry_over.reset);
        assert_eq!(carry_over.carried, 0);
        assert!(resting.member(hash(0), &conditions(&[predicate])).is_none());
    }

    #[test]
    fn set_is_emptied_when_pre_block_writes_are_unknown() {
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);

        assert!(resting.begin_block(PARENT, 2, None).reset);
        assert!(resting.is_empty());
    }

    #[test]
    fn unfinished_block_does_not_hand_on_its_members() {
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        // Block 2 is never finished, so block 3 cannot trust the set even on a matching hash.
        assert!(resting.begin_block(PARENT, 3, Some(&TransitionState::default())).reset);
    }

    #[test]
    fn pre_block_writes_release_members_reading_the_written_state() {
        let watched = balance_at_least(WATCHED, 1);
        let unrelated = storage_equals(UNRELATED, 7, 1);
        let mut resting = rested_in_parent(&[watched.clone(), unrelated.clone()]);
        let mut changes = balance_transition(WATCHED, 0, 5);
        changes.transitions.extend(storage_transition(UNRELATED, 8, 0, 1).transitions);

        let carry_over = resting.begin_block(PARENT, 2, Some(&changes));

        assert_eq!(carry_over.released_by_state, 1);
        assert!(resting.member(hash(0), &conditions(&[watched])).is_none());
        assert!(resting.member(hash(1), &conditions(&[unrelated])).is_some());
    }

    #[test]
    fn pre_block_storage_write_releases_members_on_that_slot() {
        let predicate = storage_equals(WATCHED, 7, 1);
        let mut resting = rested_in_parent(std::slice::from_ref(&predicate));

        let carry_over =
            resting.begin_block(PARENT, 2, Some(&storage_transition(WATCHED, 7, 0, 1)));

        assert_eq!(carry_over.released_by_state, 1);
        assert!(resting.is_empty());
    }

    #[test]
    fn block_number_blockers_are_released_every_block() {
        let not_yet = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(5),
        };
        let mut resting = rested_in_parent(&[not_yet]);

        let carry_over = resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        assert_eq!(carry_over.released_by_block_number, 1);
        assert!(resting.is_empty());
    }

    /// Builds blocks `2..=last_block` on top of [`rested_in_parent`], holding `held` in blocks up
    /// to `held_through`, and finishes each one through `store`. Returns the set after
    /// `last_block`.
    fn carry_through(
        store: &RestingPredicateStore,
        resting: RestingPredicates,
        held: &ValidityConditions,
        held_through: u64,
        last_block: u64,
    ) -> RestingPredicates {
        store.put(store.finish(resting), PARENT);
        let mut parent = PARENT;
        for block in 2..=last_block {
            let mut resting = store.take(32, WAIT);
            let carry_over = resting.begin_block(parent, block, Some(&TransitionState::default()));
            assert!(!carry_over.reset, "block {block} must follow the finished block");
            if block <= held_through {
                assert_eq!(resting.hold(hash(0), held), Some(true));
            }
            parent = B256::with_last_byte(block as u8);
            store.put(store.finish(resting), parent);
        }
        store.take(32, WAIT)
    }

    #[test]
    fn members_not_yielded_for_the_retention_window_are_dropped() {
        let seen = balance_at_least(WATCHED, 1);
        let unseen = balance_at_least(UNRELATED, 1);
        let held = conditions(std::slice::from_ref(&seen));
        let store = RestingPredicateStore::default();
        let window = RESTING_UNSEEN_RETAINED_BLOCKS;
        // A member last seen in block 1 is dropped when block 1 + window finishes.
        let rested = || rested_in_parent(&[seen.clone(), unseen.clone()]);
        assert_eq!(carry_through(&store, rested(), &held, u64::MAX, window).len(), 2);
        let resting = carry_through(&store, rested(), &held, u64::MAX, 1 + window);
        assert_eq!(resting.len(), 1);
        assert!(resting.member(hash(0), &held).is_some());
    }

    #[test]
    fn member_seen_after_it_was_filed_is_dropped_a_window_after_it_was_last_seen() {
        let predicate = balance_at_least(WATCHED, 1);
        let held = conditions(std::slice::from_ref(&predicate));
        let store = RestingPredicateStore::default();
        let window = RESTING_UNSEEN_RETAINED_BLOCKS;
        let last_seen = 5;
        let rested = || rested_in_parent(std::slice::from_ref(&predicate));
        // The sweep at block 1 + window re-files the member under block 5 instead of dropping it.
        assert_eq!(
            carry_through(&store, rested(), &held, last_seen, last_seen + window - 1).len(),
            1
        );
        assert!(carry_through(&store, rested(), &held, last_seen, last_seen + window).is_empty());
    }

    #[test]
    fn resting_again_in_the_same_block_lists_the_member_once() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = RestingPredicates::new(32);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        resting.rest(hash(0), predicate.clone());
        resting.rest(hash(0), predicate);
        assert_eq!(resting.len(), 1);
        assert_eq!(resting.generations.get(&2).map(Vec::len), Some(1));
    }

    #[test]
    fn block_start_never_sweeps_unseen_members() {
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);
        let block = 2 * RESTING_UNSEEN_RETAINED_BLOCKS;
        // The member was last seen in block 1, long enough ago to be swept, but block start is on
        // the path to flashblock scheduling and must leave the sweep to the end of a block.
        let carry_over = resting.begin_block(PARENT, block, Some(&TransitionState::default()));
        assert_eq!(carry_over, RestingCarryOver { carried: 1, ..Default::default() });
        assert!(resting.sweep_due());
        assert_eq!(resting.sweep_unseen(), 1);
    }

    #[test]
    fn sweep_runs_on_the_task_executor_and_hands_the_set_on() {
        let runtime = Runtime::test();
        let store = RestingPredicateStore::new(runtime);
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);
        let block = 2 * RESTING_UNSEEN_RETAINED_BLOCKS;
        resting.begin_block(PARENT, block, Some(&TransitionState::default()));
        resting.rest(hash(1), balance_at_least(UNRELATED, 1));

        let finishing = store.finish(resting);
        assert!(matches!(finishing.0, Finishing::Sweeping(_)));
        store.put(finishing, BUILT);

        let mut resting = store.take(32, WAIT);
        assert_eq!(resting.len(), 1, "the member unseen since block 1 is swept");
        let carry_over = resting.begin_block(BUILT, block + 1, Some(&TransitionState::default()));
        assert_eq!(carry_over, RestingCarryOver { carried: 1, ..Default::default() });
    }

    #[test]
    fn finishing_a_block_without_a_due_sweep_does_not_spawn() {
        let store = RestingPredicateStore::new(Runtime::test());
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        assert!(matches!(store.finish(resting).0, Finishing::Ready(_)));
    }

    #[test]
    fn finishing_an_emptied_set_drops_its_stale_generations() {
        let store = RestingPredicateStore::new(Runtime::test());
        let mut resting = RestingPredicates::new(32);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        resting.rest(hash(0), balance_at_least(WATCHED, 1));
        assert!(resting.remove(hash(0)));

        let Finishing::Ready(resting) = store.finish(resting).0 else {
            panic!("an empty set must not spawn a sweep");
        };
        assert!(resting.generations.is_empty());
    }

    #[test]
    fn build_stops_waiting_for_a_slow_sweep_and_starts_empty() {
        let limit = Duration::from_millis(25);
        let store = RestingPredicateStore::new(Runtime::test());
        let (sender, receiver) = mpsc::sync_channel(1);
        store.put(FinishingRestingPredicates(Finishing::Sweeping(receiver)), PARENT);

        let start = Instant::now();
        let mut resting = store.take(32, limit);
        assert!(start.elapsed() >= limit);
        assert!(resting.is_empty());
        let carry_over = resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        assert_eq!(carry_over, RestingCarryOver::default());
        drop(sender);
    }

    #[test]
    fn readded_hash_without_the_recorded_predicate_is_not_a_member() {
        let mut resting = rested_in_parent(&[balance_at_least(WATCHED, 1)]);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        assert!(resting.member(hash(0), &conditions(&[balance_at_least(UNRELATED, 1)])).is_none());
    }

    #[test]
    fn expired_batch_is_not_a_member() {
        let blocker = balance_at_least(WATCHED, 1);
        let deadline = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::ONE,
        };
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        assert!(resting.member(hash(0), &conditions(&[deadline, blocker])).is_none());
    }

    #[test]
    fn committed_state_wakes_members_reading_it() {
        let predicate = balance_at_least(WATCHED, 1);
        let resting = rested_in_parent(std::slice::from_ref(&predicate));
        let mut account = Account::default();
        account.info.balance = U256::from(3);

        assert_eq!(resting.woken_by_state(&EvmState::from_iter([(WATCHED, account)])), [hash(0)]);
    }

    #[test]
    fn flashblock_index_predicates_do_not_rest() {
        let mut resting = RestingPredicates::new(32);
        resting.rest(
            hash(0),
            ValidityPredicate::FlashblockIndex { op: ValidityOperator::Equal, value: U256::ONE },
        );
        assert!(resting.is_empty());
    }

    #[test]
    fn store_hands_the_set_to_the_next_job() {
        let store = RestingPredicateStore::default();
        let mut resting = store.take(32, WAIT);
        resting.begin_block(B256::ZERO, 1, Some(&TransitionState::default()));
        resting.rest(hash(0), balance_at_least(WATCHED, 1));
        store.put(store.finish(resting), PARENT);

        assert_eq!(store.take(32, WAIT).len(), 1);
        assert!(store.take(32, WAIT).is_empty(), "a taken set leaves the store empty");
    }

    /// The lane whose root is the member `hash(0)` in these tests.
    const ROOT: SelectionRoot = SelectionRoot::Lane(Address::repeat_byte(0x11), U256::ZERO);

    /// Lends `shared`'s view to `create`, as the pool does while creating an iterator.
    fn create_with_view(shared: &SharedRestingPredicates, create: impl FnOnce(&mut SelectionView)) {
        let mut hold = shared.iterator_hold::<BasePooledTransaction>();
        let mut view = hold.take_selection_view().expect("a resting set lends its view");
        create(&mut view);
        hold.restore_selection_view(view);
    }

    /// `resting` after an iterator creation held [`ROOT`] dormant.
    fn held_dormant(resting: RestingPredicates) -> RestingPredicates {
        let shared = SharedRestingPredicates::new(resting);
        create_with_view(&shared, |view| {
            view.rebuild([ROOT].into_iter());
            view.hold(ROOT, hash(0));
        });
        shared.take_predicates()
    }

    #[test]
    fn a_dormant_member_outlives_the_unseen_window_until_the_pool_removes_it() {
        let predicate = balance_at_least(WATCHED, 1);
        let held = conditions(std::slice::from_ref(&predicate));
        let store = RestingPredicateStore::default();
        let resting = held_dormant(rested_in_parent(std::slice::from_ref(&predicate)));

        let resting = carry_through(&store, resting, &held, 1, 3 * RESTING_UNSEEN_RETAINED_BLOCKS);
        assert!(resting.member(hash(0), &held).is_some(), "no build saw it, yet it stays");

        let shared = SharedRestingPredicates::new(resting);
        create_with_view(&shared, |view| view.refresh(&[ROOT], |_| None));
        assert!(shared.is_empty(), "the pool reported the root removed");
    }

    #[test]
    fn a_woken_dormant_member_rejoins_the_unseen_window() {
        let predicate = balance_at_least(WATCHED, 1);
        let held = conditions(std::slice::from_ref(&predicate));
        let store = RestingPredicateStore::default();
        let shared = SharedRestingPredicates::new(held_dormant(rested_in_parent(&[predicate])));
        create_with_view(&shared, |view| view.refresh(&[ROOT], |_| Some(hash(0))));

        let resting = shared.take_predicates();
        let window = RESTING_UNSEEN_RETAINED_BLOCKS;
        assert!(carry_through(&store, resting, &held, 1, 1 + window).is_empty());
    }

    #[test]
    fn a_released_member_is_awake_in_the_view() {
        let not_yet = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(5),
        };
        let mut resting = held_dormant(rested_in_parent(&[not_yet]));
        assert_eq!(resting.view.dormant_len(), 1);

        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        assert_eq!(resting.view.dormant_len(), 0);
        assert_eq!(resting.view.awake(), &[ROOT]);
    }

    #[test]
    fn a_reset_set_discards_its_view() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = held_dormant(rested_in_parent(&[predicate]));

        assert!(resting.begin_block(B256::repeat_byte(0x99), 2, None).reset);
        assert_eq!(resting.view.dormant_len(), 0);
        assert!(resting.view.awake().is_empty());
    }

    #[test]
    fn a_commit_hands_back_a_dormant_member_for_promotion() {
        let predicate = balance_at_least(WATCHED, 1);
        let shared = SharedRestingPredicates::new(held_dormant(rested_in_parent(&[predicate])));
        let _iterator_hold = shared.iterator_hold::<BasePooledTransaction>();
        let mut account = Account::default();
        account.info.balance = U256::from(3);

        let woken = shared.wake_by_state(&EvmState::from_iter([(WATCHED, account)]));
        assert_eq!(woken, [hash(0)], "the iterator skipped it as dormant and can promote it");
        assert!(shared.is_empty());
    }

    #[test]
    fn dormant_roots_skipped_at_creation_are_counted_apart_from_parked_ones() {
        let predicate = balance_at_least(WATCHED, 1);
        let shared = SharedRestingPredicates::new(held_dormant(rested_in_parent(&[predicate])));
        shared.take_stats();
        create_with_view(&shared, SelectionView::record_skipped);
        let stats = shared.take_stats();
        assert_eq!((stats.parked, stats.dormant_skipped), (0, 1));
    }

    /// A tail change journals the lane, or the journal overflows and the view is rebuilt: either
    /// way creation wakes the dormant root and then holds it again, so it ends dormant.
    #[test]
    fn a_root_woken_by_the_journal_and_held_again_stays_dormant_past_the_unseen_window() {
        let predicate = balance_at_least(WATCHED, 1);
        let held = conditions(std::slice::from_ref(&predicate));
        for rebuilt in [false, true] {
            let resting = held_dormant(rested_in_parent(std::slice::from_ref(&predicate)));
            let shared = SharedRestingPredicates::new(resting);
            create_with_view(&shared, |view| {
                if rebuilt {
                    view.rebuild([ROOT].into_iter());
                } else {
                    view.refresh(&[ROOT], |_| Some(hash(0)));
                }
                view.hold(ROOT, hash(0));
            });
            let store = RestingPredicateStore::default();
            let last_block = 3 * RESTING_UNSEEN_RETAINED_BLOCKS;
            let resting = carry_through(&store, shared.take_predicates(), &held, 1, last_block);
            assert!(
                resting.member(hash(0), &held).is_some(),
                "rebuilt {rebuilt}: the final dormancy keeps the member out of the unseen sweep"
            );

            let shared = SharedRestingPredicates::new(resting);
            let _iterator_hold = shared.iterator_hold::<BasePooledTransaction>();
            let mut account = Account::default();
            account.info.balance = U256::from(3);
            let woken = shared.wake_by_state(&EvmState::from_iter([(WATCHED, account)]));
            assert_eq!(woken, [hash(0)], "rebuilt {rebuilt}: its state change still promotes it");
        }
    }

    /// A member rests on a balance predicate while its full conditions also end at block 1. The
    /// next block is built before the pool's canonical maintenance expires it, so only the resting
    /// set can tell that it must be offered and classified expired, as an asked hold would.
    #[test]
    fn a_dormant_member_whose_full_conditions_expire_is_offered_at_the_next_block() {
        let blocker = balance_at_least(WATCHED, 1);
        let deadline = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::ONE,
        };
        let full = conditions(&[blocker.clone(), deadline]);
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        assert_eq!(resting.hold(hash(0), &full), Some(false), "held while building block 1");
        let mut resting = held_dormant(resting);

        let carry_over = resting.begin_block(PARENT, 2, Some(&TransitionState::default()));
        assert_eq!(carry_over.released_by_block_number, 0, "it rests on its balance predicate");
        assert_eq!(resting.hold(hash(0), &full), None, "an asked hold declines the expired member");
        assert_eq!(
            resting.view.awake(),
            &[ROOT],
            "creation must ask about it instead of skipping it as dormant"
        );
        assert_eq!(resting.len(), 1, "the builder, not block start, removes the expired member");
    }

    #[test]
    fn re_holding_a_dormant_member_lists_its_deadline_once() {
        let blocker = balance_at_least(WATCHED, 1);
        let deadline = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(50),
        };
        let full = conditions(&[blocker.clone(), deadline]);
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        assert_eq!(resting.hold(hash(0), &full), Some(false));
        let shared = SharedRestingPredicates::new(held_dormant(resting));
        for _ in 0..3 {
            create_with_view(&shared, |view| {
                view.refresh(&[ROOT], |_| Some(hash(0)));
                view.hold(ROOT, hash(0));
            });
        }
        let resting = shared.take_predicates();
        assert_eq!(resting.deadlines.values().map(|bucket| bucket.len()).sum::<usize>(), 1);
        assert_eq!(resting.deadlines.keys().copied().collect::<Vec<_>>(), [51]);
    }

    #[test]
    fn a_dormant_member_without_a_block_bound_lists_no_deadline() {
        let blocker = balance_at_least(WATCHED, 1);
        let full = conditions(std::slice::from_ref(&blocker));
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        assert_eq!(resting.hold(hash(0), &full), Some(false));
        let resting = held_dormant(resting);
        assert!(resting.deadlines.is_empty());
    }

    /// A dormant member with a distant deadline is woken by its state, removed and rests again,
    /// over and over, as a busy lineage does. Only one member is ever dormant, so the deadlines it
    /// leaves behind must not pile up until that distant block.
    #[test]
    fn deadlines_stay_bounded_by_dormant_members_under_removal_and_re_rest_churn() {
        const CYCLES: usize = 64;
        let blocker = balance_at_least(WATCHED, 1);
        let distant = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(1_000_000),
        };
        let full = conditions(&[blocker.clone(), distant]);
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        for _ in 0..CYCLES {
            assert_eq!(resting.hold(hash(0), &full), Some(false));
            resting = held_dormant(resting);
            assert!(resting.remove(hash(0)), "a state change wakes and removes it");
            resting.rest(hash(0), blocker.clone());
        }
        let listed: usize = resting.deadlines.values().map(|bucket| bucket.len()).sum();
        assert!(listed <= 1, "{listed} deadline entries retained for at most one dormant member");
    }

    fn block_bound(max: u64) -> ValidityPredicate {
        ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(max),
        }
    }

    /// Builds blocks `first..=last` on `resting` without pool maintenance, returning at each block
    /// whether the root was awake in the view after block start.
    fn awake_by_block(resting: &mut RestingPredicates, first: u64, last: u64) -> Vec<(u64, bool)> {
        (first..=last)
            .map(|block| {
                let parent = B256::left_padding_from(&block.to_be_bytes());
                resting.end_block(parent);
                resting.begin_block(parent, block, Some(&TransitionState::default()));
                (block, resting.view.awake().contains(&ROOT))
            })
            .collect()
    }

    /// `hash(0)` dormant after resting on `blocker` in block 1 with a block bound of `first`, then
    /// re-admitted under the same hash with a bound of `second`: its lane is journaled, so the next
    /// creation wakes it, the hold sees the new conditions and holds it again.
    fn re_admitted_with_another_bound(
        blocker: &ValidityPredicate,
        first: u64,
        second: u64,
    ) -> RestingPredicates {
        let mut resting = rested_in_parent(std::slice::from_ref(blocker));
        let before = conditions(&[blocker.clone(), block_bound(first)]);
        assert_eq!(resting.hold(hash(0), &before), Some(false));
        let mut resting = held_dormant(resting);
        let after = conditions(&[blocker.clone(), block_bound(second)]);
        assert_eq!(resting.hold(hash(0), &after), Some(false));
        let shared = SharedRestingPredicates::new(resting);
        create_with_view(&shared, |view| {
            view.refresh(&[ROOT], |_| Some(hash(0)));
            view.hold(ROOT, hash(0));
        });
        shared.take_predicates()
    }

    #[test]
    fn a_re_admitted_member_with_an_earlier_bound_is_offered_at_its_new_deadline() {
        let blocker = balance_at_least(WATCHED, 1);
        let mut resting = re_admitted_with_another_bound(&blocker, 50, 2);
        assert_eq!(awake_by_block(&mut resting, 2, 3), [(2, false), (3, true)]);
    }

    #[test]
    fn a_re_admitted_member_with_a_later_bound_is_offered_at_its_new_deadline() {
        let blocker = balance_at_least(WATCHED, 1);
        let mut resting = re_admitted_with_another_bound(&blocker, 2, 50);
        // Waking early only asks the hold again; the root must be awake by its new deadline.
        let awake = awake_by_block(&mut resting, 2, 51);
        assert_eq!(
            awake.last(),
            Some(&(51, true)),
            "the old deadline must not consume the new one"
        );
    }

    #[test]
    fn finishing_a_set_whose_members_all_left_releases_their_deadlines() {
        let blocker = balance_at_least(WATCHED, 1);
        let full = conditions(&[blocker.clone(), block_bound(1_000_000)]);
        let mut resting = rested_in_parent(std::slice::from_ref(&blocker));
        assert_eq!(resting.hold(hash(0), &full), Some(false));
        let mut resting = held_dormant(resting);
        assert!(resting.remove(hash(0)));

        let store = RestingPredicateStore::default();
        store.put(store.finish(resting), PARENT);
        assert!(store.take(32, WAIT).deadlines.is_empty());
    }
}
