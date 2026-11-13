//! Dormant validity transactions: those the validity pool keeps out of the best-transactions
//! snapshot of the build that owns its dormancy index, until a write or a block height can satisfy
//! them.

use std::{collections::BTreeMap, fmt, str::FromStr, sync::atomic::AtomicBool};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{AddressSet, B256Map, B256Set, HashMap, HashSet, IndexSet},
};
use parking_lot::Mutex;
use revm::{database::TransitionState, state::EvmState};

use crate::{
    ParkedPredicateIndex, ValidityOperator, ValidityPredicate, ValidityPredicateKey,
    two_d_nonce_pool::LaneId,
};

/// Most queued transactions one classification pass takes; the rest wait for the next pass.
pub const DORMANCY_CLASSIFICATION_BUDGET: usize = 32_768;

/// Transactions a classification pass takes, evaluates and installs at a time, so it holds the
/// validity pool's write lock only briefly and stops soon after its tip moves.
pub const DORMANCY_CLASSIFICATION_CHUNK: usize = 256;

/// Most state locations the owning build's [`WriteFence`] records; past it every location counts
/// as written.
pub const DORMANCY_FENCE_CAPACITY: usize = 65_536;

/// Where the native builder keeps the validity transactions it parked from one block to the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValidityDormancyMode {
    /// The builder carries them in its own resting set.
    #[default]
    Resting,
    /// The validity pool keeps them dormant, out of the owning build's snapshot.
    Pool,
}

impl fmt::Display for ValidityDormancyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Resting => "resting",
            Self::Pool => "pool",
        })
    }
}

impl FromStr for ValidityDormancyMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "resting" => Ok(Self::Resting),
            "pool" => Ok(Self::Pool),
            _ => Err("expected resting or pool"),
        }
    }
}

/// The transactions a build left parked, each with the predicate it failed.
pub type BuildParks = Vec<(TxHash, ValidityPredicate)>;

/// Ends a [`DormancyBuild`]: with the sealed block's hash and the build's parks, or `None` when the
/// build ends unsealed.
type FinishBuild = Box<dyn FnOnce(Option<(B256, BuildParks)>) + Send>;

/// One build's ownership of the validity pool's dormancy index, from
/// `ParkableTransactionPool::begin_dormancy_build` until [`Self::seal`]. Dropping it unsealed, as a
/// cancelled or failed build does, discards the index.
pub struct DormancyBuild {
    id: u64,
    parked: BuildParks,
    finish: Option<FinishBuild>,
}

impl DormancyBuild {
    /// Build `id`'s ownership; `finish` receives the sealed block's hash with the build's parks, or
    /// `None` when the build is dropped unsealed.
    pub fn new(id: u64, finish: impl FnOnce(Option<(B256, BuildParks)>) + Send + 'static) -> Self {
        Self { id, parked: Vec::new(), finish: Some(Box::new(finish)) }
    }

    /// The build's id in the pool that started it.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Records that transaction `hash` still fails `predicate` where the build stopped selecting.
    pub fn park(&mut self, hash: TxHash, predicate: ValidityPredicate) {
        self.parked.push((hash, predicate));
    }

    /// Records that the build sealed `block_hash`, which becomes the tip of the index, and makes
    /// its parks dormant on top of it.
    pub fn seal(mut self, block_hash: B256) {
        if let Some(finish) = self.finish.take() {
            finish(Some((block_hash, std::mem::take(&mut self.parked))));
        }
    }
}

impl Drop for DormancyBuild {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(None);
        }
    }
}

impl std::fmt::Debug for DormancyBuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DormancyBuild")
            .field("id", &self.id)
            .field("parked", &self.parked.len())
            .finish_non_exhaustive()
    }
}

/// Where a dormant transaction sits in the validity pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DormantSlot {
    /// The root of a lane anchored at canonical state.
    Lane(LaneId),
    /// A nonce-free transaction, by replay id.
    NonceFree(B256),
}

/// The state locations the owning build wrote on top of the tip, whose value a classification that
/// read the tip's state may have missed.
#[derive(Debug, Default)]
pub(crate) struct WriteFence {
    keys: HashSet<ValidityPredicateKey>,
    destroyed_storage: AddressSet,
    overflowed: bool,
}

impl WriteFence {
    /// Records the locations a commit that wrote `state` changed.
    pub(crate) fn record_state(&mut self, state: &EvmState) {
        for (address, account) in state.iter().filter(|(_, account)| account.is_touched()) {
            if self.overflowed {
                return;
            }
            let slots = account.storage.iter().filter(|(_, slot)| slot.is_changed());
            self.record(*address, account.is_selfdestructed(), slots.map(|(slot, _)| *slot));
        }
    }

    /// Records the locations `transitions` changed relative to the state it started from.
    pub(crate) fn record_transitions(&mut self, transitions: &TransitionState) {
        for (address, transition) in &transitions.transitions {
            if self.overflowed {
                return;
            }
            let destroyed = transition.storage_was_destroyed || transition.status.was_destroyed();
            let slots = transition.storage.iter().filter(|(_, slot)| slot.is_changed());
            self.record(*address, destroyed, slots.map(|(slot, _)| *slot));
        }
    }

    /// Records the changed `slots` of `address`, and its balance and nonce, which count as
    /// written whenever the account is touched.
    fn record(
        &mut self,
        address: Address,
        storage_destroyed: bool,
        slots: impl Iterator<Item = U256>,
    ) {
        if storage_destroyed {
            self.destroyed_storage.insert(address);
        }
        let account =
            [ValidityPredicateKey::Nonce(address), ValidityPredicateKey::Balance(address)];
        for key in account
            .into_iter()
            .chain(slots.map(|slot| ValidityPredicateKey::Storage(address, slot)))
        {
            self.keys.insert(key);
            if self.keys.len() + self.destroyed_storage.len() > DORMANCY_FENCE_CAPACITY {
                *self = Self { overflowed: true, ..Self::default() };
                return;
            }
        }
    }

    /// Whether a classification of `predicate` against the tip's state may be stale.
    pub(crate) fn voids(&self, predicate: &ValidityPredicate) -> bool {
        let key = ValidityPredicateKey::for_predicate(predicate);
        self.overflowed
            || self.keys.contains(&key)
            || matches!(key, ValidityPredicateKey::Storage(address, _) if self.destroyed_storage.contains(&address))
    }
}

/// The validity pool's dormancy index and the build lifecycle that scopes it to one tip; see the
/// crate README.
#[derive(Debug, Default)]
pub(crate) struct Dormancy {
    /// The predicate each dormant transaction is blocked on, for state wakes.
    index: ParkedPredicateIndex<()>,
    /// Where each dormant transaction sits, and the block that releases a block-number blocker.
    entries: B256Map<(DormantSlot, Option<u64>)>,
    /// Dormant transactions blocked on a block number, by the first block that satisfies it.
    block_releases: BTreeMap<u64, B256Set>,
    lanes: HashMap<LaneId, TxHash>,
    /// Pooled transactions waiting for classification against the tip.
    pub queue: IndexSet<TxHash>,
    /// What the owning build wrote on top of the tip. Behind its own lock so a commit records into
    /// it while holding the pool's read lock.
    pub fence: Mutex<WriteFence>,
    /// Whether the owning build took its snapshot, which a later classification may find in it.
    /// Atomic because the snapshot is taken under the pool's read lock.
    pub owner_snapshot_taken: AtomicBool,
    /// Transactions classified dormant after the owning build took its snapshot.
    pub classified_mid_build: B256Set,
    /// The last block this node sealed; `None` before a seal and after a discard.
    pub tip: Option<BlockNumHash>,
    /// The build that owns the index.
    pub owner: Option<u64>,
    /// Builds started so far, which is the latest one's id.
    pub builds: u64,
    /// The number of the block the latest build builds.
    pub height: u64,
}

impl Dormancy {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn is_dormant(&self, hash: &TxHash) -> bool {
        self.entries.contains_key(hash)
    }

    pub(crate) fn lane_root(&self, lane_id: &LaneId) -> Option<TxHash> {
        self.lanes.get(lane_id).copied()
    }

    /// Indexes `hash`, dormant in `slot`, under `predicate`, which fails for block `block`.
    pub(crate) fn park(
        &mut self,
        hash: TxHash,
        slot: DormantSlot,
        predicate: ValidityPredicate,
        block: u64,
    ) {
        self.remove(hash);
        let release = match &predicate {
            ValidityPredicate::BlockNumber { op: ValidityOperator::NotEqual, .. } => {
                Some(block.saturating_add(1))
            }
            ValidityPredicate::BlockNumber { op: ValidityOperator::GreaterThan, value } => {
                Some(value.saturating_to::<u64>().saturating_add(1))
            }
            ValidityPredicate::BlockNumber { value, .. } => Some(value.saturating_to()),
            _ => None,
        };
        if let Some(block) = release {
            self.block_releases.entry(block).or_default().insert(hash);
        }
        if let DormantSlot::Lane(lane_id) = slot {
            self.lanes.insert(lane_id, hash);
        }
        self.index.park(hash, (), predicate);
        self.entries.insert(hash, (slot, release));
    }

    /// Forgets `hash`, returning where it was dormant, or `None` when it was not.
    pub(crate) fn remove(&mut self, hash: TxHash) -> Option<DormantSlot> {
        let (slot, release) = self.entries.remove(&hash)?;
        if let DormantSlot::Lane(lane_id) = slot {
            self.lanes.remove(&lane_id);
        }
        if let Some(block) = release
            && let Some(hashes) = self.block_releases.get_mut(&block)
        {
            hashes.remove(&hash);
            if hashes.is_empty() {
                self.block_releases.remove(&block);
            }
        }
        self.index.remove(hash);
        Some(slot)
    }

    /// Ends the owning build's ownership, forgetting what it wrote and classified.
    pub(crate) fn end_build(&mut self) {
        self.owner = None;
        self.fence = Mutex::default();
        self.owner_snapshot_taken = AtomicBool::default();
        self.classified_mid_build.clear();
    }

    /// Empties the index, returning where each transaction was dormant.
    pub(crate) fn take_slots(&mut self) -> Vec<DormantSlot> {
        self.index = ParkedPredicateIndex::default();
        self.block_releases.clear();
        self.lanes.clear();
        self.entries.drain().map(|(_, (slot, _))| slot).collect()
    }

    /// The dormant transactions whose block-number blocker `block` satisfies.
    pub(crate) fn released_by(&mut self, block: u64) -> Vec<TxHash> {
        let pending = self.block_releases.split_off(&block.saturating_add(1));
        std::mem::replace(&mut self.block_releases, pending).into_values().flatten().collect()
    }

    /// The dormant transactions a commit that wrote `state` may satisfy.
    pub(crate) fn affected_by_state(&self, state: &EvmState) -> Vec<TxHash> {
        self.index.affected_by_state(state).affected_transactions
    }

    /// The dormant transactions the writes `transitions` hold may satisfy.
    pub(crate) fn affected_by_transitions(&self, transitions: &TransitionState) -> Vec<TxHash> {
        self.index.affected_by_transitions(transitions).affected_transactions
    }
}
