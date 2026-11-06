//! Bookkeeping for dormant validity transactions: those the validity pool keeps out of the
//! best-transactions snapshot until a state change or block advance can satisfy them.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    str::FromStr,
    sync::Arc,
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash, U256,
    map::{AddressSet, B256Set, HashMap, HashSet},
};
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
    /// Records `written`; past [`DORMANCY_FENCE_CAPACITY`] keys every location counts as written.
    pub(crate) fn record(&mut self, written: impl IntoIterator<Item = WrittenLocation>) {
        if self.overflowed {
            return;
        }
        for location in written {
            match location {
                WrittenLocation::AllStorage(address) => {
                    self.storage.insert(address);
                }
                WrittenLocation::Key(_) if self.keys.len() >= DORMANCY_FENCE_CAPACITY => {
                    self.overflowed = true;
                    return;
                }
                WrittenLocation::Key(key) => {
                    self.keys.insert(key);
                }
            }
        }
    }

    /// Adds every location `other` recorded, deduplicated and within the same cap: past it,
    /// every location counts as written.
    pub(crate) fn absorb(&mut self, other: Self) {
        self.overflowed |= other.overflowed;
        self.storage.extend(other.storage);
        self.record(other.keys.into_iter().map(WrittenLocation::Key));
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
    index: ParkedPredicateIndex<DormantEntry>,
}

impl DiscardedIndex {
    /// Where each discarded transaction sat. Walks the whole index, so call it without locks.
    pub(crate) fn into_slots(self) -> Vec<(TxHash, DormantSlot)> {
        self.index
            .hashes()
            .filter_map(|hash| Some((hash, self.index.transaction(hash)?.slot)))
            .collect()
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
    pub index: ParkedPredicateIndex<DormantEntry>,
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
        self.index.len()
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
        self.discarded.push(DiscardedIndex { index: std::mem::take(&mut self.index) });
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
        self.index.transaction(*hash).is_some()
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
        let entry = DormantEntry { slot, release_block, indexed_at: self.snapshots };
        self.index.park(hash, entry, predicate);
        if let DormantSlot::Lane(lane_id) = slot {
            self.lanes.insert(lane_id, hash);
        }
        if let Some(block) = release_block {
            self.block_releases.entry(block).or_default().insert(hash);
        }
        self.canary.push_back(hash);
        if self.canary.len() > 2 * self.index.len() + DORMANCY_CANARY_BUDGET {
            let mut seen = B256Set::default();
            let index = &self.index;
            self.canary.retain(|hash| index.transaction(*hash).is_some() && seen.insert(*hash));
        }
    }

    /// Forgets an indexed transaction, returning its entry.
    pub(crate) fn remove(&mut self, hash: TxHash) -> Option<DormantEntry> {
        let entry = self.index.remove(hash)?;
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
