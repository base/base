//! Persistent selection state that lets an enforcing build skip the validity pool's dormant
//! resting roots without visiting them; see the crate README.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{HashMap, HashSet},
};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};

/// Entries a [`SelectionJournal`] retains. A reader further behind rebuilds from scratch.
///
/// An initial bound: whether it covers the changes between two iterator creations at production
/// admission rates has not been measured.
pub const SELECTION_JOURNAL_CAPACITY: usize = 16_384;

/// A root of the validity pool: the first pending transaction of a finite lane, or a nonce-free
/// transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionRoot {
    /// The lane `(sender, nonce key)`, with the protocol nonce as key zero.
    Lane(Address, U256),
    /// The nonce-free transaction with this replay id.
    NonceFree(B256),
}

/// A root's consecutive pending run, root first. Shared, never modified.
pub type PendingRun<T> = Arc<[Arc<ValidPoolTransaction<T>>]>;

/// Where a reader of a [`SelectionJournal`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalPosition {
    journal: u64,
    sequence: u64,
}

/// Bounded, ordered log of the roots whose pending run, anchor, shared flag or bridged standard
/// successors changed. Readers replay what they missed instead of rescanning the pool.
#[derive(Debug)]
pub struct SelectionJournal {
    /// Distinguishes journals, so a position from a replaced pool is never replayed.
    id: u64,
    /// Sequence number of the next entry.
    next: u64,
    entries: VecDeque<SelectionRoot>,
}

static JOURNAL_IDS: AtomicU64 = AtomicU64::new(0);

impl Default for SelectionJournal {
    fn default() -> Self {
        Self { id: JOURNAL_IDS.fetch_add(1, Ordering::Relaxed), next: 0, entries: VecDeque::new() }
    }
}

impl SelectionJournal {
    /// Records that `root` changed, dropping the oldest entry once full.
    pub fn record(&mut self, root: SelectionRoot) {
        if self.entries.len() == SELECTION_JOURNAL_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(root);
        self.next += 1;
    }

    /// The position right after the latest entry.
    pub const fn position(&self) -> JournalPosition {
        JournalPosition { journal: self.id, sequence: self.next }
    }

    /// The entries recorded since `position`, or `None` when some of them were already dropped
    /// or `position` belongs to another journal.
    pub fn since(&self, position: JournalPosition) -> Option<impl Iterator<Item = &SelectionRoot>> {
        let first = self.next - self.entries.len() as u64;
        if position.journal != self.id || !(first..=self.next).contains(&position.sequence) {
            return None;
        }
        Some(self.entries.range(usize::try_from(position.sequence - first).ok()?..))
    }
}

/// The validity pool's pending runs by root in a persistent map, so a build snapshots every run
/// in constant time, together with the journal of their changes.
#[derive(Debug)]
pub struct SelectionIndex<T: PoolTransaction> {
    runs: imbl::HashMap<SelectionRoot, PendingRun<T>>,
    journal: SelectionJournal,
}

impl<T: PoolTransaction> Default for SelectionIndex<T> {
    fn default() -> Self {
        Self { runs: imbl::HashMap::default(), journal: SelectionJournal::default() }
    }
}

impl<T: PoolTransaction> SelectionIndex<T> {
    /// Stores `root`'s current pending run, or forgets the root when it has none, and journals it.
    pub fn update(&mut self, root: SelectionRoot, run: Option<PendingRun<T>>) {
        match run {
            Some(run) => self.runs.insert(root, run),
            None => self.runs.remove(&root),
        };
        self.journal.record(root);
    }

    /// The pending runs by root.
    pub const fn runs(&self) -> &imbl::HashMap<SelectionRoot, PendingRun<T>> {
        &self.runs
    }

    /// The journal of root changes, also used for changes the run map does not show.
    pub const fn journal(&self) -> &SelectionJournal {
        &self.journal
    }

    /// See [`Self::journal`].
    pub const fn journal_mut(&mut self) -> &mut SelectionJournal {
        &mut self.journal
    }
}

/// Standard transactions queued behind a shared validity lane's pending run, in nonce order.
pub type BridgedRun<T> = Arc<[Arc<ValidPoolTransaction<T>>]>;

/// The standard runs bridged behind shared validity lanes, kept as a persistent map that iterator
/// creation refreshes from a [`SelectionJournal`] and shares in constant time.
#[derive(Debug)]
pub struct BridgeMirror<T: PoolTransaction> {
    position: Option<JournalPosition>,
    runs: imbl::HashMap<(Address, U256), BridgedRun<T>>,
}

impl<T: PoolTransaction> Default for BridgeMirror<T> {
    fn default() -> Self {
        Self { position: None, runs: imbl::HashMap::default() }
    }
}

impl<T: PoolTransaction> BridgeMirror<T> {
    /// Recomputes the lanes journaled since the last refresh with `run_of`, or every lane in
    /// `shared_lanes` when the journal no longer covers that, and returns a snapshot.
    pub fn refresh(
        &mut self,
        journal: &SelectionJournal,
        shared_lanes: impl Iterator<Item = (Address, U256)>,
        mut run_of: impl FnMut((Address, U256)) -> Option<BridgedRun<T>>,
    ) -> imbl::HashMap<(Address, U256), BridgedRun<T>> {
        let lanes: HashSet<(Address, U256)> =
            match self.position.and_then(|position| journal.since(position)) {
                Some(entries) => entries
                    .filter_map(|root| match root {
                        SelectionRoot::Lane(sender, nonce_key) => Some((*sender, *nonce_key)),
                        SelectionRoot::NonceFree(_) => None,
                    })
                    .collect(),
                None => {
                    self.runs.clear();
                    shared_lanes.collect()
                }
            };
        for lane in lanes {
            match run_of(lane) {
                Some(run) => self.runs.insert(lane, run),
                None => self.runs.remove(&lane),
            };
        }
        self.position = Some(journal.position());
        self.runs.clone()
    }
}

/// What synchronizing with the pool and holding roots did to a [`SelectionView`] since its owner
/// last took the changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionChanges {
    /// The view could not replay the pool's journal and was rebuilt.
    pub reset: bool,
    /// Roots a hold held at creation; later iterators skip them without asking.
    pub dormant: Vec<TxHash>,
    /// Dormant roots awake again because their lane changed or the view was rebuilt, still the
    /// root of their lane.
    pub woken: Vec<TxHash>,
    /// Dormant roots that left the pool or stopped being their lane's root.
    pub removed: Vec<TxHash>,
    /// Dormant roots iterator creations skipped without asking the hold.
    pub skipped: u64,
}

/// How a [`SelectionView`] catches up with the pool; see [`SelectionView::plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionSync {
    /// The journal no longer covers the view: re-learn every root.
    Rebuild,
    /// Re-check these roots.
    Changed(Vec<SelectionRoot>),
}

/// Selection state one build lineage keeps across the pool iterators it creates.
///
/// A root a hold held at creation becomes dormant: later iterators skip it, its lane and the
/// standard transactions bridged behind it without asking the hold, until the pool journals a
/// change to its lane or the owner wakes it. Every other root is awake, and iterating the awake
/// roots costs their live count. Only root identities are stored, so the view is not tied to a
/// transaction type and can be carried by a resting set between jobs.
#[derive(Debug, Default)]
pub struct SelectionView {
    position: Option<JournalPosition>,
    awake: Vec<SelectionRoot>,
    awake_slots: HashMap<SelectionRoot, usize>,
    dormant: HashMap<SelectionRoot, TxHash>,
    /// Shared with iterators in constant time, so a same-build wake finds a dormant root.
    dormant_roots: imbl::HashMap<TxHash, SelectionRoot>,
    changes: SelectionChanges,
}

impl SelectionView {
    /// Starts catching up with `journal`: returns the roots to re-check, or a rebuild when the
    /// view is new or the journal dropped entries it had not seen.
    pub fn plan(&mut self, journal: &SelectionJournal) -> SelectionSync {
        let changed = self.position.and_then(|position| journal.since(position));
        let sync = match changed {
            Some(entries) => SelectionSync::Changed(entries.copied().collect()),
            None => {
                self.changes.reset |= self.position.is_some();
                SelectionSync::Rebuild
            }
        };
        self.position = Some(journal.position());
        sync
    }

    /// Forgets every root and makes each of `roots` awake. Dormant roots are reported woken.
    pub fn rebuild(&mut self, roots: impl Iterator<Item = SelectionRoot>) {
        self.changes.woken.extend(self.dormant.drain().map(|(_, hash)| hash));
        self.dormant_roots = imbl::HashMap::default();
        self.awake.clear();
        self.awake_slots.clear();
        for root in roots {
            self.insert_awake(root);
        }
    }

    /// Re-checks `changed` roots against `root_hash`, the current root transaction of each, or
    /// `None` for a root no longer in the pool.
    pub fn refresh(
        &mut self,
        changed: &[SelectionRoot],
        root_hash: impl Fn(&SelectionRoot) -> Option<TxHash>,
    ) {
        for root in changed {
            let current = root_hash(root);
            if let Some(hash) = self.dormant.remove(root) {
                self.dormant_roots.remove(&hash);
                if current == Some(hash) {
                    self.changes.woken.push(hash);
                } else {
                    self.changes.removed.push(hash);
                }
            }
            if current.is_some() {
                self.insert_awake(*root);
            } else {
                self.remove_awake(root);
            }
        }
    }

    /// The awake roots.
    pub fn awake(&self) -> &[SelectionRoot] {
        &self.awake
    }

    /// The number of dormant roots.
    pub fn dormant_len(&self) -> usize {
        self.dormant.len()
    }

    /// The dormant roots by root transaction hash.
    pub fn dormant_roots(&self) -> imbl::HashMap<TxHash, SelectionRoot> {
        self.dormant_roots.clone()
    }

    /// Makes `root`, whose root transaction `hash` a hold held, dormant.
    pub fn hold(&mut self, root: SelectionRoot, hash: TxHash) {
        self.remove_awake(&root);
        self.dormant.insert(root, hash);
        self.dormant_roots.insert(hash, root);
        self.changes.dormant.push(hash);
    }

    /// Makes the dormant root `hash` awake, so the next iterator asks about it again. Returns
    /// whether it was dormant.
    pub fn wake(&mut self, hash: TxHash) -> bool {
        let Some(root) = self.dormant_roots.remove(&hash) else { return false };
        self.dormant.remove(&root);
        self.insert_awake(root);
        true
    }

    /// Records that an iterator creation skipped the dormant roots.
    pub fn record_skipped(&mut self) {
        self.changes.skipped += self.dormant.len() as u64;
    }

    /// Takes the changes recorded since the last call.
    pub fn take_changes(&mut self) -> SelectionChanges {
        std::mem::take(&mut self.changes)
    }

    fn insert_awake(&mut self, root: SelectionRoot) {
        if self.dormant.contains_key(&root) || self.awake_slots.contains_key(&root) {
            return;
        }
        self.awake_slots.insert(root, self.awake.len());
        self.awake.push(root);
    }

    fn remove_awake(&mut self, root: &SelectionRoot) {
        let Some(slot) = self.awake_slots.remove(root) else { return };
        self.awake.swap_remove(slot);
        if let Some(moved) = self.awake.get(slot) {
            self.awake_slots.insert(*moved, slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(byte: u8) -> SelectionRoot {
        SelectionRoot::Lane(Address::repeat_byte(byte), U256::ZERO)
    }

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    fn sorted(roots: &[SelectionRoot]) -> Vec<SelectionRoot> {
        let mut roots = roots.to_vec();
        roots.sort_by_key(|root| format!("{root:?}"));
        roots
    }

    /// A view synced with `journal` over `roots`, all awake.
    fn synced(journal: &SelectionJournal, roots: &[SelectionRoot]) -> SelectionView {
        let mut view = SelectionView::default();
        assert_eq!(view.plan(journal), SelectionSync::Rebuild);
        view.rebuild(roots.iter().copied());
        view
    }

    #[test]
    fn a_reader_replays_only_what_it_missed() {
        let mut journal = SelectionJournal::default();
        journal.record(lane(1));
        let position = journal.position();
        journal.record(lane(2));
        journal.record(lane(3));
        let missed: Vec<_> = journal.since(position).unwrap().copied().collect();
        assert_eq!(missed, vec![lane(2), lane(3)]);
        assert_eq!(journal.since(journal.position()).unwrap().count(), 0);
    }

    #[test]
    fn a_reader_behind_the_retained_entries_or_on_another_journal_must_rebuild() {
        let mut journal = SelectionJournal::default();
        let position = journal.position();
        for _ in 0..SELECTION_JOURNAL_CAPACITY {
            journal.record(lane(1));
        }
        assert_eq!(journal.since(position).unwrap().count(), SELECTION_JOURNAL_CAPACITY);
        journal.record(lane(2));
        assert!(journal.since(position).is_none(), "one entry past capacity forces a rebuild");
        assert!(SelectionJournal::default().since(journal.position()).is_none());
    }

    #[test]
    fn an_overflowed_view_reports_a_reset_and_wakes_every_dormant_root() {
        let mut journal = SelectionJournal::default();
        let mut view = synced(&journal, &[lane(1), lane(2)]);
        view.hold(lane(1), hash(1));
        for _ in 0..=SELECTION_JOURNAL_CAPACITY {
            journal.record(lane(3));
        }

        assert_eq!(view.plan(&journal), SelectionSync::Rebuild);
        view.rebuild([lane(1), lane(2)].into_iter());
        let changes = view.take_changes();
        assert!(changes.reset);
        assert_eq!(changes.woken, vec![hash(1)]);
        assert_eq!(sorted(view.awake()), sorted(&[lane(1), lane(2)]));
        assert_eq!(view.dormant_len(), 0);
    }

    #[test]
    fn a_dormant_root_is_skipped_until_its_lane_changes() {
        let mut journal = SelectionJournal::default();
        let mut view = synced(&journal, &[lane(1), lane(2)]);
        view.hold(lane(1), hash(1));
        assert_eq!(view.awake(), &[lane(2)]);
        assert_eq!(view.plan(&journal), SelectionSync::Changed(Vec::new()));

        journal.record(lane(1));
        let SelectionSync::Changed(changed) = view.plan(&journal) else { panic!("replayable") };
        view.refresh(&changed, |_| Some(hash(1)));
        assert_eq!(sorted(view.awake()), sorted(&[lane(1), lane(2)]));
        let changes = view.take_changes();
        assert_eq!((changes.dormant, changes.woken), (vec![hash(1)], vec![hash(1)]));
    }

    #[test]
    fn a_replaced_or_removed_dormant_root_is_reported_removed() {
        let mut journal = SelectionJournal::default();
        let mut view = synced(&journal, &[lane(1), lane(2)]);
        view.hold(lane(1), hash(1));
        view.hold(lane(2), hash(2));
        journal.record(lane(1));
        journal.record(lane(2));
        let SelectionSync::Changed(changed) = view.plan(&journal) else { panic!("replayable") };

        view.refresh(&changed, |root| (*root == lane(1)).then(|| hash(9)));
        assert_eq!(view.awake(), &[lane(1)], "the replaced root is awake, the removed one gone");
        let mut removed = view.take_changes().removed;
        removed.sort();
        assert_eq!(removed, vec![hash(1), hash(2)]);
    }

    #[test]
    fn a_woken_root_is_awake_and_no_longer_shared_with_iterators() {
        let journal = SelectionJournal::default();
        let mut view = synced(&journal, &[lane(1)]);
        view.hold(lane(1), hash(1));
        let shared = view.dormant_roots();
        assert!(view.wake(hash(1)));
        assert!(!view.wake(hash(1)));
        assert_eq!(view.awake(), &[lane(1)]);
        assert!(view.dormant_roots().is_empty());
        assert_eq!(shared.get(&hash(1)), Some(&lane(1)), "an existing iterator keeps its snapshot");
    }

    #[test]
    fn awake_roots_stay_dense_after_removals() {
        let journal = SelectionJournal::default();
        let mut view = synced(&journal, &[lane(1), lane(2), lane(3)]);
        view.refresh(&[lane(1)], |_| None);
        view.hold(lane(3), hash(3));
        assert_eq!(view.awake(), &[lane(2)]);
        view.refresh(&[lane(4)], |_| Some(hash(4)));
        assert_eq!(sorted(view.awake()), sorted(&[lane(2), lane(4)]));
    }
}
