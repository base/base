//! Bookkeeping for dormant validity transactions: those the validity pool keeps out of the
//! best-transactions snapshot until a state change or block advance can satisfy them.

use std::{collections::BTreeMap, fmt, str::FromStr, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash, U256,
    map::{AddressSet, B256Map, B256Set, HashMap, HashSet, IndexSet},
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

/// Most queued transactions one classification pass takes; the rest wait for the next pass, so a
/// pass after a discard re-queued every dormant transaction does not hold up canonical
/// maintenance for long.
pub const DORMANCY_CLASSIFICATION_BUDGET: usize = 32_768;

/// Candidates classified between checks that the pass's results still count, so a pass whose
/// results a discard or an overflowing fence voided stops reading state soon after.
pub const DORMANCY_CLASSIFICATION_CHUNK: usize = 256;

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

/// The validity pool's predicate index, scoped to the builder's tip, with the classification
/// queue and the build lifecycle that keeps every entry false on top of the tip.
///
/// Owned by the pool it classifies, which keeps its ready indexes in step with every change.
#[derive(Debug)]
pub(crate) struct Dormancy {
    mode: ValidityDormancyMode,
    /// The predicate each dormant transaction is blocked on, for wakes.
    pub index: ParkedPredicateIndex<()>,
    /// The dormant transactions.
    entries: B256Map<DormantEntry>,
    block_releases: BTreeMap<u64, B256Set>,
    lanes: HashMap<LaneId, TxHash>,
    /// Pooled transactions waiting for classification; a transaction leaving the pool leaves it.
    queue: IndexSet<TxHash>,
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
    /// Discards so far, so a classification can tell the index it read is gone.
    pub discards: u64,
    /// `discards` when the classification in flight started.
    pub classifying: Option<u64>,
    /// Locations builds wrote on top of the state classification reads, besides those of the
    /// running build, which it keeps itself: pre-selection writes and those of builds sealed
    /// while a classification was in flight, deduplicated and capped at
    /// [`DORMANCY_FENCE_CAPACITY`], past which the classification parks nothing.
    pub fence: WriteFence,
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
            tip: None,
            builds: 0,
            building: false,
            snapshots: 0,
            height: 0,
            deferred_discard: None,
            discards: 0,
            classifying: None,
            fence: WriteFence::default(),
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

    /// Empties the index, its block releases, its lane roots and the fence, returning the
    /// transactions that were dormant, which the caller makes ready again.
    pub(crate) fn take_entries(&mut self) -> B256Map<DormantEntry> {
        self.index = ParkedPredicateIndex::default();
        self.block_releases = BTreeMap::new();
        self.lanes = HashMap::default();
        self.clear_fences();
        std::mem::take(&mut self.entries)
    }

    /// Forgets every location builds wrote.
    pub(crate) fn clear_fences(&mut self) {
        self.fence = WriteFence::default();
    }

    /// Records that a sealed build wrote `written` on top of the state a classification in
    /// flight reads.
    pub(crate) fn fence_sealed(&mut self, written: WriteFence) {
        self.fence.absorb(written);
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
}
