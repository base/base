//! Bookkeeping for dormant validity transactions: those the validity pool keeps out of the
//! best-transactions snapshot until a state change or block advance can satisfy them.

use std::{
    collections::BTreeMap,
    fmt,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash, U256,
    map::{AddressSet, B256Map, B256Set, HashMap, HashSet, IndexSet},
};
use parking_lot::{Condvar, Mutex};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use revm::{Database, state::EvmState};

use crate::{
    ParkedPredicateIndex, PredicateContext, ValidityConditions, ValidityOperator,
    ValidityPredicate, ValidityPredicateEvaluation, ValidityPredicateKey, WrittenLocation,
    two_d_nonce_pool::LaneId,
};

/// Most state locations builds record for classification before parking stops until the fence
/// is cleared.
pub const DORMANCY_FENCE_CAPACITY: usize = 65_536;

/// Most queued transactions one classification pass takes; the rest wait for the next pass, so a
/// pass after a discard re-queued every dormant transaction does not hold up canonical
/// maintenance for long.
pub const DORMANCY_CLASSIFICATION_BUDGET: usize = 32_768;

/// Dormant transactions the canary re-evaluates per classification.
pub const DORMANCY_CANARY_BUDGET: usize = 64;

/// Most classification results installed per hold of the validity pool lock, so a build waiting
/// on the lock never waits long.
pub const DORMANCY_INSTALL_BATCH: usize = 256;

/// Longest a build starting waits for the merge of the previous build's parks before running
/// without the dormancy index.
pub const DORMANCY_MERGE_WAIT_LIMIT: Duration = Duration::from_millis(100);

/// Tells a build starting whether the parks of the last sealed build are still being merged into
/// the dormancy index off the build path, and lets it wait for the merge to end.
#[derive(Debug, Default)]
pub(crate) struct DormancyMergeGate {
    merges: Mutex<MergeCounts>,
    finished: Condvar,
}

/// The merges started and finished so far. At most one is pending: the sealing build keeps the
/// dormancy index until its merge ends, so no other build can seal meanwhile.
#[derive(Debug, Default)]
struct MergeCounts {
    started: u64,
    finished: u64,
}

impl MergeCounts {
    const fn is_pending(&self) -> bool {
        self.finished < self.started
    }
}

impl DormancyMergeGate {
    /// Records that a merge started; [`Self::finish`] ends it.
    pub(crate) fn start(&self) {
        self.merges.lock().started += 1;
    }

    /// Records that the pending merge ended and wakes the builds waiting for it.
    pub(crate) fn finish(&self) {
        self.merges.lock().finished += 1;
        self.finished.notify_all();
    }

    /// Whether a merge is pending.
    pub(crate) fn is_pending(&self) -> bool {
        self.merges.lock().is_pending()
    }

    /// Waits up to `limit` for the merges started so far to end, returning how long it waited,
    /// or `None` when no merge was pending.
    pub(crate) fn wait(&self, limit: Duration) -> Option<Duration> {
        let mut merges = self.merges.lock();
        if !merges.is_pending() {
            return None;
        }
        let started_so_far = merges.started;
        let started = Instant::now();
        self.finished.wait_while_for(&mut merges, |merges| merges.finished < started_so_far, limit);
        Some(started.elapsed())
    }
}

/// Whether the validity pool keeps transactions whose predicates cannot hold yet out of the
/// best-transactions snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValidityDormancyMode {
    /// Classify nothing; every validity transaction is offered to builds.
    #[default]
    Off,
    /// Keep dormant transactions out of the best-transactions snapshot.
    Enforce,
}

impl ValidityDormancyMode {
    /// Whether dormant transactions are kept out of builds.
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

impl fmt::Display for ValidityDormancyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Enforce => "enforce",
        })
    }
}

impl FromStr for ValidityDormancyMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "enforce" => Ok(Self::Enforce),
            _ => Err("expected off or enforce"),
        }
    }
}

/// The outcome of evaluating a validity transaction for dormancy against one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DormancyClassification {
    /// Every predicate holds.
    Matched,
    /// Dormancy does not apply: the state could not be read or the block bounds have passed.
    Undetermined,
    /// The transaction cannot run in the next block.
    Dormant {
        /// The predicate that blocks it.
        predicate: ValidityPredicate,
        /// For a block-number blocker, the first block that can satisfy it.
        release_block: Option<u64>,
    },
}

impl DormancyClassification {
    /// Evaluates `conditions` against `db` for the block `context` describes.
    pub fn of<DB: Database>(
        conditions: &ValidityConditions,
        db: &mut DB,
        context: &PredicateContext,
    ) -> Self {
        let Ok(evaluation) = ValidityPredicateEvaluation::evaluate(conditions, db, context) else {
            return Self::Undetermined;
        };
        let predicate = match evaluation {
            ValidityPredicateEvaluation::Matched => return Self::Matched,
            ValidityPredicateEvaluation::Unsatisfied { expired: true, .. } => {
                return Self::Undetermined;
            }
            ValidityPredicateEvaluation::Unsatisfied { predicate, .. } => predicate,
        };
        let release_block = match &predicate {
            ValidityPredicate::BlockNumber { op, value } => {
                match Self::release_block(*op, *value, context.block_number) {
                    Some(block) => Some(block),
                    None => return Self::Undetermined,
                }
            }
            _ => None,
        };
        Self::Dormant { predicate, release_block }
    }

    /// The first block after `block` that satisfies `block op value`, for the operators that
    /// can become true as blocks advance.
    pub(crate) fn release_block(op: ValidityOperator, value: U256, block: u64) -> Option<u64> {
        let value = value.saturating_to::<u64>();
        match op {
            ValidityOperator::GreaterThanOrEqual | ValidityOperator::Equal => Some(value),
            ValidityOperator::GreaterThan => Some(value.saturating_add(1)),
            ValidityOperator::NotEqual => Some(block.saturating_add(1)),
            ValidityOperator::LessThan | ValidityOperator::LessThanOrEqual => None,
        }
    }
}

/// Why the validity pool discarded its dormancy index, making every dormant transaction ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DormancyDiscardReason {
    /// A build started on a parent other than the tip.
    ParentMismatch,
    /// A build ended without sealing a block.
    Abandoned,
    /// The canonical block at the tip's height is not the tip.
    CanonicalMismatch,
    /// The canonical chain reorged.
    Reorg,
    /// The canonical-state feed skipped notifications.
    FeedGap,
    /// The background merge of a sealed build's parks panicked or was cancelled.
    MergeFailed,
}

impl DormancyDiscardReason {
    /// The metric label for this reason.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ParentMismatch => "parent_mismatch",
            Self::Abandoned => "abandoned",
            Self::CanonicalMismatch => "canonical_mismatch",
            Self::Reorg => "reorg",
            Self::FeedGap => "feed_gap",
            Self::MergeFailed => "merge_failed",
        }
    }
}

/// Where a dormant transaction sits in the validity pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DormantSlot {
    /// The anchored root of a lane.
    Lane(LaneId),
    /// A nonce-free transaction, by replay id.
    NonceFree(B256),
}

/// One indexed transaction.
#[derive(Debug)]
pub(crate) struct DormantEntry {
    /// Where it is dormant.
    pub slot: DormantSlot,
    release_block: Option<u64>,
    /// [`Dormancy::snapshots`] when it was indexed: a snapshot with a higher id never held it.
    pub indexed_at: u64,
}

/// State locations builds wrote on top of the state a classification reads, so it does not park
/// a transaction on a location that may since have changed.
#[derive(Debug, Default)]
pub(crate) struct WriteFence {
    keys: HashSet<ValidityPredicateKey>,
    storage: AddressSet,
    overflowed: bool,
}

impl WriteFence {
    /// Records `written`. Keys and destroyed-storage addresses share one cap of
    /// [`DORMANCY_FENCE_CAPACITY`]; past it every location counts as written and nothing more is
    /// recorded.
    pub(crate) fn record(&mut self, written: impl IntoIterator<Item = WrittenLocation>) {
        for location in written {
            if self.overflowed {
                return;
            }
            if self.keys.len() + self.storage.len() >= DORMANCY_FENCE_CAPACITY {
                self.record_overflow();
                return;
            }
            match location {
                WrittenLocation::AllStorage(address) => {
                    self.storage.insert(address);
                }
                WrittenLocation::Key(key) => {
                    self.keys.insert(key);
                }
            }
        }
    }

    /// Adds every location `other` recorded, deduplicated and within the same cap: past it,
    /// every location counts as written and nothing more is absorbed.
    pub(crate) fn absorb(&mut self, other: Self) {
        if self.overflowed {
            return;
        }
        if other.overflowed {
            self.record_overflow();
            return;
        }
        let storage = other.storage.into_iter().map(WrittenLocation::AllStorage);
        self.record(storage.chain(other.keys.into_iter().map(WrittenLocation::Key)));
    }

    /// Makes every location count as written, dropping what was recorded.
    fn record_overflow(&mut self) {
        self.overflowed = true;
        self.keys = HashSet::default();
        self.storage = AddressSet::default();
    }

    /// Whether every location counts as written.
    pub(crate) const fn is_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Whether the location `predicate` reads was written.
    pub(crate) fn contains(&self, predicate: &ValidityPredicate) -> bool {
        let key = ValidityPredicateKey::for_predicate(predicate);
        self.overflowed
            || self.keys.contains(&key)
            || matches!(key, ValidityPredicateKey::Storage(address, _) if self.storage.contains(&address))
    }
}

/// What one build parked and wrote, kept with the build instead of in the validity pool so the
/// build loop never takes the pool's lock to park. Parks still held when the build seals are
/// merged into the pool's index, since they fail on top of the sealed block.
#[derive(Debug)]
pub(crate) struct BuildLocal<T: PoolTransaction> {
    /// Transactions the build found failing a predicate and no later commit woke, under it.
    pub index: ParkedPredicateIndex<()>,
    pub parked: B256Map<(Arc<ValidPoolTransaction<T>>, ValidityPredicate)>,
    /// Locations the build wrote; recorded only when it builds on the tip.
    pub written: Option<WriteFence>,
}

impl<T: PoolTransaction> BuildLocal<T> {
    pub(crate) fn new(records_writes: bool) -> Self {
        Self {
            index: ParkedPredicateIndex::default(),
            parked: B256Map::default(),
            written: records_writes.then(WriteFence::default),
        }
    }

    /// Parks `transaction` under the failing `predicate`.
    pub(crate) fn park(
        &mut self,
        transaction: &Arc<ValidPoolTransaction<T>>,
        predicate: ValidityPredicate,
    ) {
        let hash = *transaction.hash();
        self.index.remove(hash);
        self.index.park(hash, (), predicate.clone());
        self.parked.insert(hash, (Arc::clone(transaction), predicate));
    }

    /// The parked transactions with the predicates they failed. Walks every park and frees the
    /// build's predicate index, so call it without locks.
    pub(crate) fn into_parks(self) -> Vec<(Arc<ValidPoolTransaction<T>>, ValidityPredicate)> {
        self.parked.into_values().collect()
    }

    /// Records what a commit wrote and returns the parked transactions it may satisfy, which are
    /// no longer parked here.
    pub(crate) fn wake(&mut self, state: &EvmState) -> Vec<Arc<ValidPoolTransaction<T>>> {
        if let Some(written) = self.written.as_mut() {
            written.record(WrittenLocation::of_state(state));
        }
        if self.parked.is_empty() {
            return Vec::new();
        }
        let woken = self.index.affected_by_state(state).affected_transactions;
        woken
            .into_iter()
            .filter_map(|hash| {
                self.index.remove(hash);
                self.parked.remove(&hash).map(|(transaction, _)| transaction)
            })
            .collect()
    }
}

/// An index a discard set aside, whose transactions are made ready again off the build path.
#[derive(Debug, Default)]
pub(crate) struct DiscardedIndex {
    _index: ParkedPredicateIndex<()>,
    _block_releases: BTreeMap<u64, B256Set>,
    _lanes: HashMap<LaneId, TxHash>,
    entries: B256Map<DormantEntry>,
}

impl DiscardedIndex {
    /// Where each discarded transaction sat. Walks the whole index, so call it without locks.
    pub(crate) fn into_slots(self) -> Vec<(TxHash, DormantSlot)> {
        self.entries.into_iter().map(|(hash, entry)| (hash, entry.slot)).collect()
    }
}

/// Collections the validity pool retired under its lock, to free after releasing it.
#[derive(Debug, Default)]
pub(crate) struct RetiredCollections {
    canaries: Vec<IndexSet<TxHash>>,
    fences: Vec<WriteFence>,
}

/// The validity pool's predicate index, scoped to the builder's tip, with the classification
/// queue and the build lifecycle that keeps every entry false on top of the tip.
///
/// Owned by the pool it classifies, which keeps its ready indexes in step with every change.
#[derive(Debug)]
pub(crate) struct Dormancy {
    mode: ValidityDormancyMode,
    /// The predicate each indexed transaction is blocked on, for wakes. It may also hold, for
    /// the length of a merge, a sealed build's parks not yet confirmed dormant.
    pub index: ParkedPredicateIndex<()>,
    /// The dormant transactions: those in `index` confirmed to be dormant.
    entries: B256Map<DormantEntry>,
    block_releases: BTreeMap<u64, B256Set>,
    lanes: HashMap<LaneId, TxHash>,
    /// Pooled transactions waiting for classification; a transaction leaving the pool leaves it.
    queue: IndexSet<TxHash>,
    /// Dormant transactions blocked on state, sampled in rotation from `canary_next`.
    canary: IndexSet<TxHash>,
    canary_next: usize,
    /// The last block this builder sealed; `None` until a seal and after a discard.
    pub tip: Option<BlockNumHash>,
    /// Builds started so far; the latest one is `builds`.
    pub builds: u64,
    /// Whether build `builds` still owns the index.
    pub building: bool,
    /// Snapshots taken for builds so far; the id of the latest one.
    pub snapshots: u64,
    /// The block number of the latest build started.
    pub height: u64,
    /// A discard requested while a build was running, applied when that build ends so its
    /// iterator keeps waking what it parked and what its snapshot left out.
    pub deferred_discard: Option<DormancyDiscardReason>,
    /// Indexes discards set aside whose transactions are not all ready again yet.
    discarded: Vec<DiscardedIndex>,
    /// Discarded indexes being made ready again right now.
    restoring: usize,
    /// Discards so far, so a classification can tell the index it read is gone.
    pub discards: u64,
    /// `discards` when the classification in flight started.
    pub classifying: Option<u64>,
    /// Locations builds wrote on top of the state classification reads, besides those of the
    /// running build, which it keeps itself: pre-selection writes and those of builds sealed
    /// while a classification was in flight, deduplicated and capped at
    /// [`DORMANCY_FENCE_CAPACITY`], past which the classification parks nothing.
    pub fence: WriteFence,
    /// Collections retired under the lock, freed once it is released.
    retired: RetiredCollections,
    /// Transactions woken during the running build.
    pub build_wakes: usize,
}

impl Dormancy {
    pub(crate) fn new(mode: ValidityDormancyMode) -> Self {
        Self {
            mode,
            index: ParkedPredicateIndex::default(),
            entries: B256Map::default(),
            block_releases: BTreeMap::new(),
            lanes: HashMap::default(),
            queue: IndexSet::default(),
            canary: IndexSet::default(),
            canary_next: 0,
            tip: None,
            builds: 0,
            building: false,
            snapshots: 0,
            height: 0,
            deferred_discard: None,
            discarded: Vec::new(),
            restoring: 0,
            discards: 0,
            classifying: None,
            fence: WriteFence::default(),
            retired: RetiredCollections::default(),
            build_wakes: 0,
        }
    }

    pub(crate) const fn is_enforced(&self) -> bool {
        self.mode.is_enforced()
    }

    /// Number of dormant transactions.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether `build` is the running build.
    pub(crate) const fn is_running(&self, build: u64) -> bool {
        self.building && self.builds == build
    }

    /// Whether every transaction a discard set aside is ready again, so the per-lane dormancy
    /// flags and the pending-lane indexes describe the current index.
    pub(crate) const fn is_consistent(&self) -> bool {
        self.discarded.is_empty() && self.restoring == 0
    }

    /// Sets the index, its block releases and its lane roots aside in constant time, leaving an
    /// empty index; the transactions it held are made ready again by
    /// [`Self::take_discarded`]'s caller, off the build path.
    pub(crate) fn set_index_aside(&mut self) {
        self.retired.canaries.push(std::mem::take(&mut self.canary));
        self.canary_next = 0;
        self.discarded.push(DiscardedIndex {
            _index: std::mem::take(&mut self.index),
            _block_releases: std::mem::take(&mut self.block_releases),
            _lanes: std::mem::take(&mut self.lanes),
            entries: std::mem::take(&mut self.entries),
        });
        self.clear_fences();
    }

    /// Forgets every location builds wrote, retiring the fence to free outside the lock.
    pub(crate) fn clear_fences(&mut self) {
        self.retired.fences.push(std::mem::take(&mut self.fence));
    }

    /// Records that a sealed build wrote `written` on top of the state a classification in
    /// flight reads.
    pub(crate) fn fence_sealed(&mut self, written: WriteFence) {
        self.fence.absorb(written);
    }

    /// Takes the collections retired so far, to free outside the lock.
    pub(crate) fn take_retired(&mut self) -> RetiredCollections {
        std::mem::take(&mut self.retired)
    }

    /// Takes one discarded index to make ready again; [`Self::finish_restoring`] ends it.
    pub(crate) fn take_discarded(&mut self) -> Option<DiscardedIndex> {
        let discarded = self.discarded.pop()?;
        self.restoring += 1;
        Some(discarded)
    }

    pub(crate) const fn finish_restoring(&mut self) {
        self.restoring = self.restoring.saturating_sub(1);
    }

    pub(crate) fn enqueue(&mut self, hash: TxHash) {
        if self.is_enforced() {
            self.queue.insert(hash);
        }
    }

    /// Forgets a queued transaction that left the pool.
    pub(crate) fn unqueue(&mut self, hash: &TxHash) {
        self.queue.swap_remove(hash);
    }

    /// Takes at most [`DORMANCY_CLASSIFICATION_BUDGET`] queued transactions, leaving the rest
    /// queued.
    pub(crate) fn take_queued(&mut self) -> Vec<TxHash> {
        let kept = self.queue.len().saturating_sub(DORMANCY_CLASSIFICATION_BUDGET);
        self.queue.drain(kept..).collect()
    }

    /// Number of queued transactions.
    #[cfg(test)]
    pub(crate) fn queued_len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn is_dormant(&self, hash: &TxHash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Adopts a sealed build's parks as the index in constant time when the index is empty, as
    /// it is after a discard or before the first seal. Each adopted transaction becomes dormant
    /// once [`Self::confirm_adopted`] confirms it, and is dropped from the index otherwise.
    pub(crate) fn adopt_index(&mut self, parks: &mut ParkedPredicateIndex<()>) -> bool {
        if !self.index.is_empty() || !self.entries.is_empty() {
            return false;
        }
        std::mem::swap(&mut self.index, parks);
        true
    }

    /// Whether `hash` is an adopted park still awaiting confirmation: in the index, not yet
    /// dormant.
    pub(crate) fn awaits_confirmation(&self, hash: &TxHash) -> bool {
        !self.entries.contains_key(hash) && self.index.transaction(*hash).is_some()
    }

    /// Makes an adopted park dormant in `slot`, keeping the predicate it was adopted under.
    pub(crate) fn confirm_adopted(
        &mut self,
        hash: TxHash,
        slot: DormantSlot,
        release_block: Option<u64>,
    ) {
        self.record_entry(hash, DormantEntry { slot, release_block, indexed_at: self.snapshots });
    }

    pub(crate) fn dormant_lane_root(&self, lane_id: &LaneId) -> Option<TxHash> {
        self.lanes.get(lane_id).copied()
    }

    /// Returns the block-number blockers that block `block` satisfies.
    pub(crate) fn release_through(&mut self, block: u64) -> Vec<TxHash> {
        let pending = self.block_releases.split_off(&block.saturating_add(1));
        std::mem::replace(&mut self.block_releases, pending).into_values().flatten().collect()
    }

    /// Indexes `hash` under `predicate`, replacing any entry it had.
    pub(crate) fn park(
        &mut self,
        hash: TxHash,
        slot: DormantSlot,
        predicate: ValidityPredicate,
        release_block: Option<u64>,
    ) {
        self.remove(hash);
        self.index.park(hash, (), predicate);
        self.record_entry(hash, DormantEntry { slot, release_block, indexed_at: self.snapshots });
    }

    /// Records that indexed `hash` is dormant as `entry`.
    fn record_entry(&mut self, hash: TxHash, entry: DormantEntry) {
        if let DormantSlot::Lane(lane_id) = entry.slot {
            self.lanes.insert(lane_id, hash);
        }
        if let Some(block) = entry.release_block {
            self.block_releases.entry(block).or_default().insert(hash);
        }
        // A block-number blocker is released by the build at its height, never missed by a wake.
        if entry.release_block.is_none() {
            self.canary.insert(hash);
        }
        self.entries.insert(hash, entry);
    }

    /// Forgets an indexed transaction, returning its entry, or `None` when it was not dormant.
    pub(crate) fn remove(&mut self, hash: TxHash) -> Option<DormantEntry> {
        self.index.remove(hash);
        let entry = self.entries.remove(&hash)?;
        self.canary.swap_remove(&hash);
        if let DormantSlot::Lane(lane_id) = entry.slot {
            self.lanes.remove(&lane_id);
        }
        if let Some(block) = entry.release_block
            && let Some(hashes) = self.block_releases.get_mut(&block)
        {
            hashes.remove(&hash);
            if hashes.is_empty() {
                self.block_releases.remove(&block);
            }
        }
        Some(entry)
    }

    /// The next dormant transactions in the canary's rotation, at most
    /// [`DORMANCY_CANARY_BUDGET`] of them.
    pub(crate) fn take_canary(&mut self) -> Vec<TxHash> {
        let len = self.canary.len();
        let picked = (0..DORMANCY_CANARY_BUDGET.min(len))
            .filter_map(|offset| self.canary.get_index((self.canary_next + offset) % len).copied())
            .collect::<Vec<_>>();
        self.canary_next = if len == 0 { 0 } else { (self.canary_next + picked.len()) % len };
        picked
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;

    fn address(index: usize) -> Address {
        Address::left_padding_from(&index.to_be_bytes())
    }

    fn balance_of(address: Address) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op: ValidityOperator::GreaterThan, value: U256::ZERO }
    }

    /// Block-number blockers stay out of the rotation, and the state-blocked
    /// transactions it holds are sampled again by every classification.
    #[test]
    fn the_canary_keeps_sampling_state_blocked_transactions_among_block_bound_ones() {
        let mut dormancy = Dormancy::new(ValidityDormancyMode::Enforce);
        let block_bound = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1_000),
        };
        for index in 0..110_u64 {
            let hash = B256::from(U256::from(index));
            let slot = DormantSlot::NonceFree(hash);
            if index < 10 {
                dormancy.park(hash, slot, balance_of(address(index as usize)), None);
            } else {
                dormancy.park(hash, slot, block_bound.clone(), Some(1_000));
            }
        }

        for _ in 0..2 {
            assert_eq!(dormancy.take_canary().len(), 10);
        }
        dormancy.remove(B256::from(U256::from(3)));
        let sampled = dormancy.take_canary();
        assert_eq!(sampled.len(), 9, "a transaction no longer dormant leaves the rotation");
        assert!(!sampled.contains(&B256::from(U256::from(3))));
    }

    /// Destroyed-storage addresses count against the same cap as keys; past
    /// it every location counts as written and nothing more is recorded or
    /// absorbed.
    #[test]
    fn a_fence_caps_keys_and_destroyed_storage_together_then_fences_everything() {
        let mut fence = WriteFence::default();
        fence.record(
            (0..DORMANCY_FENCE_CAPACITY / 2)
                .map(|index| WrittenLocation::AllStorage(address(index))),
        );
        let mut sealed = WriteFence::default();
        sealed.record(
            (0..DORMANCY_FENCE_CAPACITY / 2)
                .map(|index| WrittenLocation::Key(ValidityPredicateKey::Balance(address(index)))),
        );
        fence.absorb(sealed);
        assert!(!fence.contains(&balance_of(address(DORMANCY_FENCE_CAPACITY))), "at the cap");

        fence.record([WrittenLocation::AllStorage(address(DORMANCY_FENCE_CAPACITY))]);
        assert!(fence.contains(&balance_of(address(DORMANCY_FENCE_CAPACITY + 1))));
        let mut later = WriteFence::default();
        later.record([WrittenLocation::AllStorage(address(DORMANCY_FENCE_CAPACITY + 2))]);
        fence.absorb(later);
        assert!(fence.keys.is_empty() && fence.storage.is_empty(), "nothing is retained");
    }

    /// Each finish ends one started merge, so the gate stays pending until every merge started
    /// has finished.
    #[test]
    fn each_finish_ends_one_started_merge() {
        let gate = DormancyMergeGate::default();
        gate.start();
        gate.start();

        gate.finish();
        assert!(gate.is_pending(), "the earlier merge did not clear the later one");
        gate.finish();
        assert!(!gate.is_pending());
        assert!(gate.wait(Duration::from_secs(10)).is_none());
    }

    /// A build waiting on a pending merge wakes when that merge finishes, well before its limit,
    /// whether it began waiting before or after the merge finished.
    #[test]
    fn a_waiting_build_wakes_when_its_merge_finishes() {
        let gate = Arc::new(DormancyMergeGate::default());
        gate.start();
        let (done, waited) = std::sync::mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || done.send(gate.wait(Duration::from_secs(10))).unwrap())
        };

        gate.finish();
        let waited = waited.recv_timeout(Duration::from_secs(5)).expect("the waiter woke");
        assert!(waited.is_none_or(|waited| waited < Duration::from_secs(5)));
        waiter.join().unwrap();
    }
}
