//! Bookkeeping for dormant validity transactions: those the validity pool keeps out of the
//! best-transactions snapshot until a state change or block advance can satisfy them.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    str::FromStr,
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash, U256,
    map::{AddressSet, B256Set, HashMap, HashSet},
};
use revm::Database;

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
    /// A build started while another was running.
    Superseded,
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
            Self::Superseded => "superseded",
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
    /// Where it is dormant; `None` for a transaction a build indexed only for mid-block wakes.
    pub slot: Option<DormantSlot>,
    release_block: Option<u64>,
    /// [`Dormancy::snapshots`] when it was indexed: a snapshot with a higher id never held it.
    pub indexed_at: u64,
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
    /// Whether build `builds` is still running.
    pub building: bool,
    /// Snapshots taken for builds so far; the id of the latest one.
    pub snapshots: u64,
    /// The block number of the latest build started.
    pub height: u64,
    /// A discard requested while a build was running, applied when that build ends so its
    /// iterator keeps waking what it parked and what its snapshot left out.
    pub deferred_discard: Option<DormancyDiscardReason>,
    /// Entries without a slot, dropped when their build ends.
    in_build: B256Set,
    /// Discards so far, so a classification can tell the index it read is gone.
    pub discards: u64,
    /// `discards` when the classification in flight started.
    pub classifying: Option<u64>,
    /// Locations builds wrote on top of the state classification reads.
    fence: HashSet<ValidityPredicateKey>,
    fenced_storage: AddressSet,
    fence_overflowed: bool,
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
            in_build: B256Set::default(),
            discards: 0,
            classifying: None,
            fence: HashSet::default(),
            fenced_storage: AddressSet::default(),
            fence_overflowed: false,
            build_wakes: 0,
        }
    }

    pub(crate) const fn is_enforced(&self) -> bool {
        self.mode.is_enforced()
    }

    /// Number of dormant transactions.
    pub(crate) fn len(&self) -> usize {
        self.index.len() - self.in_build.len()
    }

    /// Whether `build` is the running build.
    pub(crate) const fn is_running(&self, build: u64) -> bool {
        self.building && self.builds == build
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
        self.index.transaction(*hash).is_some_and(|entry| entry.slot.is_some())
    }

    pub(crate) fn dormant_lane_root(&self, lane_id: &LaneId) -> Option<TxHash> {
        self.lanes.get(lane_id).copied()
    }

    /// Records locations a build wrote on top of the tip, for classification to avoid.
    pub(crate) fn fence(&mut self, written: impl IntoIterator<Item = WrittenLocation>) {
        if self.tip.is_none() || self.fence_overflowed {
            return;
        }
        for location in written {
            match location {
                WrittenLocation::AllStorage(address) => {
                    self.fenced_storage.insert(address);
                }
                WrittenLocation::Key(_) if self.fence.len() >= DORMANCY_FENCE_CAPACITY => {
                    self.fence_overflowed = true;
                    return;
                }
                WrittenLocation::Key(key) => {
                    self.fence.insert(key);
                }
            }
        }
    }

    pub(crate) fn clear_fence(&mut self) {
        self.fence.clear();
        self.fenced_storage.clear();
        self.fence_overflowed = false;
    }

    /// Whether a build wrote the location `predicate` reads since the state classification read.
    pub(crate) fn is_fenced(&self, predicate: &ValidityPredicate) -> bool {
        let key = ValidityPredicateKey::for_predicate(predicate);
        self.fence_overflowed
            || self.fence.contains(&key)
            || matches!(key, ValidityPredicateKey::Storage(address, _) if self.fenced_storage.contains(&address))
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
        slot: Option<DormantSlot>,
        predicate: ValidityPredicate,
        release_block: Option<u64>,
    ) {
        self.remove(hash);
        let entry = DormantEntry { slot, release_block, indexed_at: self.snapshots };
        self.index.park(hash, entry, predicate);
        match slot {
            Some(DormantSlot::Lane(lane_id)) => {
                self.lanes.insert(lane_id, hash);
            }
            Some(DormantSlot::NonceFree(_)) => {}
            None => {
                self.in_build.insert(hash);
                return;
            }
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
        match entry.slot {
            Some(DormantSlot::Lane(lane_id)) => {
                self.lanes.remove(&lane_id);
            }
            Some(DormantSlot::NonceFree(_)) => {}
            None => {
                self.in_build.remove(&hash);
            }
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

    /// The entries indexed only for the running build's mid-block wakes.
    pub(crate) fn in_build_hashes(&self) -> Vec<TxHash> {
        self.in_build.iter().copied().collect()
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
