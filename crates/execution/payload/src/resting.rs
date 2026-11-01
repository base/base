//! Validity transactions resting under an unsatisfied predicate, carried across block builds.

use std::{
    collections::BTreeMap,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{AddressSet, B256Set, HashSet},
};
use base_execution_txpool::{
    BasePooledTx, BestTransactionsHold, IngestionRest, PredicateContext, ValidityConditions,
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

    /// Returns the weaker of `self` and `cap`: off below shadow below enforce.
    pub const fn capped_by(self, cap: Self) -> Self {
        match (self, cap) {
            (Self::Off, _) | (_, Self::Off) => Self::Off,
            (Self::Enforce, Self::Enforce) => Self::Enforce,
            _ => Self::Shadow,
        }
    }
}

/// Why a pool-recorded [`IngestionRest`] cannot stand in for evaluating a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestionRestStale {
    /// The set is not tracking the keys the block writes, so the result cannot be checked.
    Untracked,
    /// The result was evaluated on a block other than the parent of the block being built.
    ParentMismatch,
    /// The block being built has already written the state the predicate reads, or its pre-block
    /// writes are unknown.
    KeyWritten,
    /// The candidate's conditions no longer contain the recorded predicate.
    PredicateAbsent,
    /// The candidate's batch is expired at the current block.
    Expired,
}

impl IngestionRestStale {
    /// Returns the `reason` label value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untracked => "untracked",
            Self::ParentMismatch => "parent_mismatch",
            Self::KeyWritten => "key_written",
            Self::PredicateAbsent => "predicate_absent",
            Self::Expired => "expired",
        }
    }
}

/// How builds used the [`IngestionRest`] results recorded by the pool.
///
/// Counted locally and recorded once per build, so the hot paths do no metric lookups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestionRestStats {
    /// Candidates whose result applied, so they rested without being evaluated (enforced) or
    /// would have (shadow).
    pub applied: u64,
    /// Candidates whose result was evaluated on a block other than the parent.
    pub stale_parent: u64,
    /// Candidates whose predicate state the block had already written.
    pub stale_written: u64,
    /// Candidates whose conditions no longer contained the recorded predicate.
    pub stale_absent: u64,
    /// Candidates whose batch was expired.
    pub stale_expired: u64,
    /// Candidates checked by a set that does not track written keys.
    pub stale_untracked: u64,
    /// Of [`Self::applied`], candidates that matched when still evaluated in shadow mode. Each
    /// would have been a missed inclusion under enforcement.
    pub shadow_mismatches: u64,
}

impl IngestionRestStats {
    /// Counts a result that did not apply for `reason`.
    pub const fn record_stale(&mut self, reason: IngestionRestStale) {
        match reason {
            IngestionRestStale::Untracked => self.stale_untracked += 1,
            IngestionRestStale::ParentMismatch => self.stale_parent += 1,
            IngestionRestStale::KeyWritten => self.stale_written += 1,
            IngestionRestStale::PredicateAbsent => self.stale_absent += 1,
            IngestionRestStale::Expired => self.stale_expired += 1,
        }
    }

    /// Adds `other`'s counts to these.
    pub const fn merge(&mut self, other: &Self) {
        self.applied += other.applied;
        self.stale_parent += other.stale_parent;
        self.stale_written += other.stale_written;
        self.stale_absent += other.stale_absent;
        self.stale_expired += other.stale_expired;
        self.stale_untracked += other.stale_untracked;
        self.shadow_mismatches += other.shadow_mismatches;
    }

    /// Returns every stale count with its reason, for recording.
    pub const fn stale_counts(&self) -> [(IngestionRestStale, u64); 5] {
        [
            (IngestionRestStale::Untracked, self.stale_untracked),
            (IngestionRestStale::ParentMismatch, self.stale_parent),
            (IngestionRestStale::KeyWritten, self.stale_written),
            (IngestionRestStale::PredicateAbsent, self.stale_absent),
            (IngestionRestStale::Expired, self.stale_expired),
        ]
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
///
/// With [`Self::track_written_keys`] on, the set also records every state key the block being
/// built has written, which lets [`Self::ingestion_rest_applies`] accept the pool's admission-time
/// evaluation of a candidate in place of evaluating it: a predicate found unsatisfied on the parent
/// stays unsatisfied until the block writes the state it reads.
#[derive(Debug)]
pub struct RestingPredicates {
    index: ParkedPredicateIndex<RestingMember>,
    ordered_threshold: usize,
    block_number: u64,
    /// Parent of the block being built.
    parent: B256,
    /// Hash of the block whose final state the members were last checked against. `None` while a
    /// block is being built, so a build that does not finish cannot hand its members on.
    tip: Option<B256>,
    /// State keys the block being built has written, while [`Self::track_written_keys`] is on.
    written: Option<Box<WrittenKeys>>,
    /// Members by the block they are filed under. A member is filed under the block that rested
    /// it, and re-filed under its latest seen block when the sweep finds it was seen since, so the
    /// sweep only visits members old enough to drop. Entries for members that were removed or
    /// re-filed since are stale and skipped.
    generations: BTreeMap<u64, Vec<TxHash>>,
}

impl RestingPredicates {
    /// Creates an empty set whose predicate buckets convert at `ordered_threshold` entries.
    pub fn new(ordered_threshold: usize) -> Self {
        Self {
            index: ParkedPredicateIndex::new(ordered_threshold),
            ordered_threshold,
            block_number: 0,
            parent: B256::ZERO,
            tip: None,
            written: None,
            generations: BTreeMap::new(),
        }
    }

    /// Sets whether the set records the state keys each block writes, which
    /// [`Self::ingestion_rest_applies`] needs. Set it before [`Self::begin_block`], whose pre-block
    /// writes are recorded only while tracking is on.
    pub fn track_written_keys(&mut self, enabled: bool) {
        self.written = enabled.then(Box::default);
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
        self.parent = parent_hash;
        if let Some(written) = self.written.as_deref_mut() {
            written.begin_block(pre_block_changes);
        }
        match pre_block_changes.filter(|_| follows_parent) {
            None => {
                carry_over.reset = !self.index.is_empty();
                self.index = ParkedPredicateIndex::new(self.ordered_threshold);
                self.generations.clear();
            }
            Some(changes) => {
                for hash in self.index.affected_by_transitions(changes).affected_transactions {
                    carry_over.released_by_state += usize::from(self.index.remove(hash).is_some());
                }
                for hash in self.index.transactions_blocked_by(ValidityPredicateKey::BlockNumber) {
                    carry_over.released_by_block_number +=
                        usize::from(self.index.remove(hash).is_some());
                }
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
                    self.index.remove(hash);
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
        let filed = self
            .index
            .entry(transaction_hash)
            .is_some_and(|(member, _)| member.filed_block == self.block_number);
        let member = RestingMember {
            rested_block: self.block_number,
            seen_block: self.block_number,
            filed_block: self.block_number,
        };
        self.index.park(transaction_hash, member, predicate);
        if !filed {
            self.generations.entry(self.block_number).or_default().push(transaction_hash);
        }
    }

    /// Removes a member, returning whether it was present.
    pub fn remove(&mut self, transaction_hash: TxHash) -> bool {
        !self.index.is_empty() && self.index.remove(transaction_hash).is_some()
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

    /// Records the state keys one committed transaction wrote, when tracking written keys.
    ///
    /// Builders call it for every commit of the block, in commit order, so that
    /// [`Self::ingestion_rest_applies`] sees every write that precedes a candidate.
    pub fn record_written(&mut self, state: &EvmState) {
        if let Some(written) = self.written.as_deref_mut() {
            written.record_state(state);
        }
    }

    /// Returns whether the pool's admission-time evaluation `rest` of a candidate with
    /// `conditions` still holds in the block being built, so the candidate can rest under
    /// `rest.predicate` without being evaluated.
    ///
    /// It holds when the result was evaluated on the parent of the block being built, the block
    /// has not written the state the predicate reads, and the candidate's batch still contains the
    /// predicate and is unexpired. Accepting it is then the same as evaluating the candidate and
    /// finding `rest.predicate` unsatisfied. Expiry is checked at the block's first flashblock, as
    /// for carried members.
    pub fn ingestion_rest_applies(
        &self,
        rest: &IngestionRest,
        conditions: &ValidityConditions,
    ) -> Result<(), IngestionRestStale> {
        let Some(written) = self.written.as_deref() else {
            return Err(IngestionRestStale::Untracked);
        };
        if rest.state_block_hash != self.parent {
            return Err(IngestionRestStale::ParentMismatch);
        }
        if !conditions.contains(&rest.predicate) {
            return Err(IngestionRestStale::PredicateAbsent);
        }
        if conditions
            .is_expired(&PredicateContext { block_number: self.block_number, flashblock_index: 0 })
        {
            return Err(IngestionRestStale::Expired);
        }
        if written.may_have_changed(ValidityPredicateKey::for_predicate(&rest.predicate)) {
            return Err(IngestionRestStale::KeyWritten);
        }
        Ok(())
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

/// State keys the block being built has written so far, for
/// [`RestingPredicates::ingestion_rest_applies`].
#[derive(Debug, Default)]
struct WrittenKeys {
    /// Whether every write of the block is recorded. False when the pre-block writes were not
    /// reported to [`RestingPredicates::begin_block`].
    complete: bool,
    /// Balance, nonce, and storage keys written so far.
    keys: HashSet<ValidityPredicateKey>,
    /// Accounts destroyed so far, whose every storage slot reads differently.
    storage_destroyed: AddressSet,
}

impl WrittenKeys {
    /// Starts a block whose `pre_block_changes` were applied before the selection loop.
    fn begin_block(&mut self, pre_block_changes: Option<&TransitionState>) {
        self.keys.clear();
        self.storage_destroyed.clear();
        self.complete = pre_block_changes.is_some();
        let Some(transitions) = pre_block_changes else { return };
        for (address, transition) in &transitions.transitions {
            let old = transition.previous_info.as_ref();
            let new = transition.info.as_ref();
            self.record_account(
                *address,
                old.map_or(U256::ZERO, |info| info.balance)
                    != new.map_or(U256::ZERO, |info| info.balance),
                old.map_or(0, |info| info.nonce) != new.map_or(0, |info| info.nonce),
                transition.storage_was_destroyed || transition.status.was_destroyed(),
                transition
                    .storage
                    .iter()
                    .filter(|(_, slot)| slot.is_changed())
                    .map(|(slot, _)| *slot),
            );
        }
    }

    /// Records one committed transaction's writes.
    fn record_state(&mut self, state: &EvmState) {
        for (address, account) in state {
            let destroyed = account.is_selfdestructed();
            let original = account.original_info();
            self.record_account(
                *address,
                destroyed || original.balance != account.info.balance,
                destroyed || original.nonce != account.info.nonce,
                destroyed,
                account.storage.iter().filter(|(_, slot)| slot.is_changed()).map(|(slot, _)| *slot),
            );
        }
    }

    fn record_account(
        &mut self,
        address: Address,
        balance_changed: bool,
        nonce_changed: bool,
        destroyed: bool,
        changed_slots: impl Iterator<Item = U256>,
    ) {
        if balance_changed {
            self.keys.insert(ValidityPredicateKey::Balance(address));
        }
        if nonce_changed {
            self.keys.insert(ValidityPredicateKey::Nonce(address));
        }
        if destroyed {
            self.storage_destroyed.insert(address);
        }
        for slot in changed_slots {
            self.keys.insert(ValidityPredicateKey::Storage(address, slot));
        }
    }

    /// Returns whether the block may have written the state `key` reads.
    fn may_have_changed(&self, key: ValidityPredicateKey) -> bool {
        !self.complete
            || match key {
                ValidityPredicateKey::Balance(_) | ValidityPredicateKey::Nonce(_) => {
                    self.keys.contains(&key)
                }
                ValidityPredicateKey::Storage(address, _) => {
                    self.keys.contains(&key) || self.storage_destroyed.contains(&address)
                }
                // Position predicates are never recorded at admission; refuse them rather than
                // guess.
                ValidityPredicateKey::BlockNumber | ValidityPredicateKey::FlashblockIndex => true,
            }
    }
}

/// A finished block's [`RestingPredicates`], possibly still being swept in the background.
#[derive(Debug)]
pub struct FinishingRestingPredicates(Finishing);

#[derive(Debug)]
enum Finishing {
    Ready(RestingPredicates),
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
            Finishing::Ready(resting) => Some(resting),
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
    /// How the iterator holds used the pool's admission-time evaluations. Every adopted
    /// transaction is also counted in [`Self::parked`].
    pub ingestion: IngestionRestStats,
    /// Time the pool iterators spent advancing past held transactions, from the first hold of
    /// each `next` call until it found a candidate.
    pub duration: Duration,
}

/// A resting set shared between one build and the pool iterators it drives.
///
/// The build loop populates the set through [`Self::rest`] and reports commits through
/// [`Self::wake_by_state`]. Each pool iterator the build creates carries a [`Self::iterator_hold`]
/// that parks members at the source, so the build loop never sees a transaction whose recorded
/// predicate is still unsatisfied. With [`Self::with_ingestion_rest_adoption`], the hold also
/// adopts a non-member whose pool admission-time evaluation still applies: it joins the set under
/// the recorded predicate and is parked without the build loop ever evaluating it. The iterator and the build loop run on the build thread, so
/// the lock is uncontended; it exists because the iterator is a boxed trait object the build loop
/// does not own.
#[derive(Debug, Clone)]
pub struct SharedRestingPredicates {
    inner: Arc<Mutex<SharedRestingState>>,
}

#[derive(Debug)]
struct SharedRestingState {
    predicates: RestingPredicates,
    /// Members parked by the current iterator's hold. The build loop promotes these when a commit
    /// wakes them; members the build loop parked itself are woken by its own predicate index.
    held: B256Set,
    stats: RestingStats,
    /// Whether the hold adopts non-members on their admission-time evaluation.
    adopt_ingestion_rest: bool,
    /// Transactions adopted since [`SharedRestingPredicates::take_adopted`] last ran, so the build
    /// loop can report their deferral, which it never otherwise sees.
    adopted: Vec<TxHash>,
}

impl SharedRestingPredicates {
    /// Shares `predicates` with the pool iterators of one build.
    pub fn new(predicates: RestingPredicates) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedRestingState {
                predicates,
                held: B256Set::default(),
                stats: RestingStats::default(),
                adopt_ingestion_rest: false,
                adopted: Vec::new(),
            })),
        }
    }

    /// Sets whether the iterator hold adopts a transaction whose pool admission-time evaluation
    /// still applies, see [`RestingPredicates::ingestion_rest_applies`]. Requires the set to
    /// track written keys; without tracking, nothing applies and nothing is adopted.
    #[must_use]
    pub fn with_ingestion_rest_adoption(self, enabled: bool) -> Self {
        self.inner.lock().adopt_ingestion_rest = enabled;
        self
    }

    /// Returns whether the pool's admission-time evaluation of a candidate still applies.
    pub fn ingestion_rest_applies(
        &self,
        rest: &IngestionRest,
        conditions: &ValidityConditions,
    ) -> Result<(), IngestionRestStale> {
        self.inner.lock().predicates.ingestion_rest_applies(rest, conditions)
    }

    /// Returns the transactions the hold adopted since the last call that are still resting, so
    /// the build loop can report their deferral once.
    pub fn take_adopted(&self) -> Vec<TxHash> {
        let mut inner = self.inner.lock();
        if inner.adopted.is_empty() {
            return Vec::new();
        }
        let mut adopted = std::mem::take(&mut inner.adopted);
        adopted.retain(|hash| inner.held.contains(hash));
        adopted
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
    /// transaction's changes, and returns those the current iterator held so the caller can
    /// promote them.
    pub fn wake_by_state(&self, state: &EvmState) -> Vec<TxHash> {
        let mut inner = self.inner.lock();
        inner.predicates.record_written(state);
        if inner.predicates.is_empty() {
            return Vec::new();
        }
        let mut held = Vec::new();
        for transaction_hash in inner.predicates.woken_by_state(state) {
            inner.predicates.remove(transaction_hash);
            if inner.held.remove(&transaction_hash) {
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
        let hash = *transaction.hash();
        let conditions = transaction.transaction.validity_conditions();
        if let Some(carried) = inner.predicates.hold(hash, conditions) {
            inner.held.insert(hash);
            inner.stats.parked += 1;
            inner.stats.carried_parked += u64::from(carried);
            return true;
        }
        if !inner.adopt_ingestion_rest {
            return false;
        }
        let Some(rest) = transaction.transaction.ingestion_rest() else { return false };
        if let Err(reason) = inner.predicates.ingestion_rest_applies(rest, conditions) {
            inner.stats.ingestion.record_stale(reason);
            return false;
        }
        // Equivalent to the build loop evaluating it here and parking it under `rest.predicate`.
        inner.predicates.rest(hash, rest.predicate.clone());
        inner.held.insert(hash);
        inner.adopted.push(hash);
        inner.stats.parked += 1;
        inner.stats.ingestion.applied += 1;
        true
    }

    fn record_duration(&mut self, duration: Duration) {
        self.shared.inner.lock().stats.duration += duration;
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
            return FinishingRestingPredicates(Finishing::Ready(resting));
        }
        if !resting.sweep_due() {
            return FinishingRestingPredicates(Finishing::Ready(resting));
        }
        let Some(executor) = &self.executor else {
            resting.sweep_unseen();
            return FinishingRestingPredicates(Finishing::Ready(resting));
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
    use base_execution_txpool::ValidityOperator;
    use revm::{
        database::{AccountStatus, TransitionAccount, TransitionState, states::StorageSlot},
        state::{Account, AccountInfo, EvmStorageSlot},
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

    /// A tracking set building block 2 on [`PARENT`] with `pre_block_changes` applied.
    fn tracking_block_two(pre_block_changes: Option<&TransitionState>) -> RestingPredicates {
        let mut resting = RestingPredicates::new(32);
        resting.track_written_keys(true);
        resting.begin_block(PARENT, 2, pre_block_changes);
        resting
    }

    fn ingestion_rest(predicate: ValidityPredicate) -> IngestionRest {
        IngestionRest { state_block_hash: PARENT, predicate }
    }

    #[test]
    fn ingestion_rest_applies_when_the_block_has_not_written_the_predicate_state() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = tracking_block_two(Some(&balance_transition(UNRELATED, 0, 5)));
        let mut account = Account::default();
        account.info.balance = U256::from(3);
        resting.record_written(&EvmState::from_iter([(UNRELATED, account)]));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(predicate.clone()),
                &conditions(&[predicate]),
            ),
            Ok(())
        );
    }

    #[test]
    fn ingestion_rest_is_stale_once_a_commit_writes_the_predicate_state() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = tracking_block_two(Some(&TransitionState::default()));
        let mut account = Account::default();
        account.info.balance = U256::from(3);
        resting.record_written(&EvmState::from_iter([(WATCHED, account)]));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(predicate.clone()),
                &conditions(&[predicate]),
            ),
            Err(IngestionRestStale::KeyWritten)
        );
    }

    #[test]
    fn ingestion_rest_is_stale_once_pre_block_writes_touch_the_predicate_state() {
        let predicate = storage_equals(WATCHED, 7, 1);
        let resting = tracking_block_two(Some(&storage_transition(WATCHED, 7, 0, 1)));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(predicate.clone()),
                &conditions(&[predicate]),
            ),
            Err(IngestionRestStale::KeyWritten)
        );
    }

    #[test]
    fn written_keys_distinguish_balance_nonce_and_slots_of_one_account() {
        let mut resting = tracking_block_two(Some(&balance_transition(WATCHED, 0, 5)));
        let mut account = Account::default();
        account.storage.insert(
            U256::from(7),
            EvmStorageSlot::new_changed(U256::ZERO, U256::ONE, Default::default()),
        );
        resting.record_written(&EvmState::from_iter([(WATCHED, account)]));

        // The pre-block write changed the balance, the commit changed slot 7, and nothing
        // changed the nonce or slot 8.
        for (predicate, expected) in [
            (balance_at_least(WATCHED, 1), Err(IngestionRestStale::KeyWritten)),
            (storage_equals(WATCHED, 7, 1), Err(IngestionRestStale::KeyWritten)),
            (storage_equals(WATCHED, 8, 1), Ok(())),
            (
                ValidityPredicate::Nonce {
                    address: WATCHED,
                    op: ValidityOperator::Equal,
                    value: U256::ONE,
                },
                Ok(()),
            ),
        ] {
            assert_eq!(
                resting.ingestion_rest_applies(
                    &ingestion_rest(predicate.clone()),
                    &conditions(std::slice::from_ref(&predicate)),
                ),
                expected,
                "{predicate:?}"
            );
        }
    }

    #[test]
    fn ingestion_rest_is_stale_when_pre_block_writes_are_unknown() {
        let predicate = balance_at_least(WATCHED, 1);
        let resting = tracking_block_two(None);

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(predicate.clone()),
                &conditions(&[predicate]),
            ),
            Err(IngestionRestStale::KeyWritten)
        );
    }

    #[test]
    fn ingestion_rest_evaluated_on_another_block_does_not_apply() {
        let predicate = balance_at_least(WATCHED, 1);
        let resting = tracking_block_two(Some(&TransitionState::default()));

        assert_eq!(
            resting.ingestion_rest_applies(
                &IngestionRest { state_block_hash: BUILT, predicate: predicate.clone() },
                &conditions(&[predicate]),
            ),
            Err(IngestionRestStale::ParentMismatch)
        );
    }

    #[test]
    fn ingestion_rest_for_a_predicate_no_longer_in_the_batch_does_not_apply() {
        let resting = tracking_block_two(Some(&TransitionState::default()));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(balance_at_least(WATCHED, 1)),
                &conditions(&[balance_at_least(UNRELATED, 1)]),
            ),
            Err(IngestionRestStale::PredicateAbsent)
        );
    }

    #[test]
    fn ingestion_rest_of_an_expired_batch_does_not_apply() {
        let blocker = balance_at_least(WATCHED, 1);
        let deadline = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::ONE,
        };
        let resting = tracking_block_two(Some(&TransitionState::default()));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(blocker.clone()),
                &conditions(&[deadline, blocker]),
            ),
            Err(IngestionRestStale::Expired)
        );
    }

    #[test]
    fn ingestion_rest_never_applies_without_written_key_tracking() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = RestingPredicates::new(32);
        resting.begin_block(PARENT, 2, Some(&TransitionState::default()));

        assert_eq!(
            resting.ingestion_rest_applies(
                &ingestion_rest(predicate.clone()),
                &conditions(&[predicate]),
            ),
            Err(IngestionRestStale::Untracked)
        );
    }

    #[test]
    fn written_keys_reset_at_each_block() {
        let predicate = balance_at_least(WATCHED, 1);
        let mut resting = tracking_block_two(Some(&balance_transition(WATCHED, 0, 5)));
        resting.end_block(BUILT);
        resting.begin_block(BUILT, 3, Some(&TransitionState::default()));

        assert_eq!(
            resting.ingestion_rest_applies(
                &IngestionRest { state_block_hash: BUILT, predicate: predicate.clone() },
                &conditions(&[predicate]),
            ),
            Ok(())
        );
    }

    #[test]
    fn mode_capping_never_exceeds_either_mode() {
        use RestingPredicateMode::{Enforce, Off, Shadow};
        assert_eq!(Enforce.capped_by(Enforce), Enforce);
        assert_eq!(Enforce.capped_by(Shadow), Shadow);
        assert_eq!(Shadow.capped_by(Enforce), Shadow);
        assert_eq!(Enforce.capped_by(Off), Off);
        assert_eq!(Off.capped_by(Enforce), Off);
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
}
