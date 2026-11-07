//! Bookkeeping for dormant validity transactions: those the validity pool keeps out of the
//! best-transactions snapshot until a state change or block advance can satisfy them.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash, U256,
    map::{AddressSet, B256Map, B256Set, HashMap, HashSet},
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
    merges: Mutex<MergeGenerations>,
    finished: Condvar,
}

/// The generations of the merges started so far and of those still pending.
#[derive(Debug, Default)]
struct MergeGenerations {
    started: u64,
    pending: BTreeSet<u64>,
}

impl DormancyMergeGate {
    /// Records that a merge started and returns its generation, which only
    /// [`Self::finish`] for that generation ends.
    pub(crate) fn start(&self) -> u64 {
        let mut merges = self.merges.lock();
        merges.started += 1;
        let generation = merges.started;
        merges.pending.insert(generation);
        generation
    }

    /// Records that the merge of `generation` ended and wakes the builds waiting for it.
    pub(crate) fn finish(&self, generation: u64) {
        self.merges.lock().pending.remove(&generation);
        self.finished.notify_all();
    }

    /// Whether a merge is pending.
    pub(crate) fn is_pending(&self) -> bool {
        !self.merges.lock().pending.is_empty()
    }

    /// Waits up to `limit` for the merges pending now to end, ignoring merges started later,
    /// returning how long it waited, or `None` when no merge was pending.
    pub(crate) fn wait(&self, limit: Duration) -> Option<Duration> {
        let mut merges = self.merges.lock();
        let latest = *merges.pending.last()?;
        let started = Instant::now();
        self.finished.wait_while_for(
            &mut merges,
            |merges| merges.pending.first().is_some_and(|oldest| *oldest <= latest),
            limit,
        );
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
    /// Dormancy does not apply: the state could not be read, the block bounds have passed, or
    /// only a flashblock-index predicate fails.
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
            ValidityPredicateEvaluation::Unsatisfied { expired: true, .. }
            | ValidityPredicateEvaluation::Unsatisfied {
                blocker: ValidityPredicateKey::FlashblockIndex,
                ..
            } => return Self::Undetermined,
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
    pub parked: HashMap<TxHash, (Arc<ValidPoolTransaction<T>>, ValidityPredicate)>,
    /// Locations the build wrote; recorded only when it builds on the tip.
    pub written: Option<WriteFence>,
}

impl<T: PoolTransaction> BuildLocal<T> {
    pub(crate) fn new(records_writes: bool) -> Self {
        Self {
            index: ParkedPredicateIndex::default(),
            parked: HashMap::default(),
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
    releases: Vec<BTreeMap<u64, B256Set>>,
    canaries: Vec<VecDeque<TxHash>>,
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
    queue: B256Set,
    canary: VecDeque<TxHash>,
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
            queue: B256Set::default(),
            canary: VecDeque::new(),
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
        self.retired.releases.push(std::mem::take(&mut self.block_releases));
        self.retired.canaries.push(std::mem::take(&mut self.canary));
        self.lanes = HashMap::default();
        self.discarded.push(DiscardedIndex {
            _index: std::mem::take(&mut self.index),
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

    pub(crate) fn take_queue(&mut self) -> B256Set {
        std::mem::take(&mut self.queue)
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
        self.entries.insert(hash, entry);
        self.canary.push_back(hash);
        if self.canary.len() > 2 * self.entries.len() + DORMANCY_CANARY_BUDGET {
            let mut seen = B256Set::default();
            let entries = &self.entries;
            self.canary.retain(|hash| entries.contains_key(hash) && seen.insert(*hash));
        }
    }

    /// Forgets an indexed transaction, returning its entry, or `None` when it was not dormant.
    pub(crate) fn remove(&mut self, hash: TxHash) -> Option<DormantEntry> {
        self.index.remove(hash);
        let entry = self.entries.remove(&hash)?;
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
        let mut picked = Vec::new();
        while picked.len() < DORMANCY_CANARY_BUDGET.min(self.len()) {
            let Some(hash) = self.canary.pop_front() else { break };
            if self.is_dormant(&hash) && !picked.contains(&hash) {
                self.canary.push_back(hash);
                picked.push(hash);
            }
        }
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

    /// A merge's completion ends only its own generation.
    #[test]
    fn finishing_a_merge_ends_only_its_own_generation() {
        let gate = DormancyMergeGate::default();
        let first = gate.start();
        let second = gate.start();

        gate.finish(first);
        assert!(gate.is_pending(), "the earlier merge did not clear the later one");
        gate.finish(second);
        assert!(!gate.is_pending());
        assert!(gate.wait(Duration::from_secs(10)).is_none());
    }

    /// A build waiting on a pending merge wakes when that merge finishes, well before its limit,
    /// whether it began waiting before or after the merge finished.
    #[test]
    fn a_waiting_build_wakes_when_its_merge_finishes() {
        let gate = Arc::new(DormancyMergeGate::default());
        let merge = gate.start();
        let (done, waited) = std::sync::mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || done.send(gate.wait(Duration::from_secs(10))).unwrap())
        };

        gate.finish(merge);
        let waited = waited.recv_timeout(Duration::from_secs(5)).expect("the waiter woke");
        assert!(waited.is_none_or(|waited| waited < Duration::from_secs(5)));
        waiter.join().unwrap();
    }
}
