//! Validity transactions resting under an unsatisfied predicate, carried across block builds.

use std::{
    collections::BTreeMap,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alloy_primitives::{B256, TxHash};
use base_execution_txpool::{PredicateContext, ValidityConditions, ValidityPredicate};
use parking_lot::Mutex;
use reth_tasks::Runtime;
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
        let member = RestingMember {
            rested_block: self.block_number,
            seen_block: self.block_number,
            filed_block: self.block_number,
        };
        self.index.park(transaction_hash, member, predicate);
        self.generations.entry(self.block_number).or_default().push(transaction_hash);
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
    Ready(RestingPredicates),
    Sweeping(mpsc::Receiver<RestingPredicates>),
}

impl FinishingRestingPredicates {
    /// Returns the set, waiting for an unfinished sweep.
    ///
    /// Returns `None` when the sweep task was dropped without returning the set. Time spent
    /// waiting is recorded, because it delays the build that takes the set.
    pub fn wait(self) -> Option<RestingPredicates> {
        match self.0 {
            Finishing::Ready(resting) => Some(resting),
            Finishing::Sweeping(receiver) => match receiver.try_recv() {
                Ok(resting) => {
                    ValidityMetrics::record_resting_sweep_wait(Duration::ZERO);
                    Some(resting)
                }
                Err(mpsc::TryRecvError::Empty) => {
                    let start = Instant::now();
                    let resting = receiver.recv().ok();
                    ValidityMetrics::record_resting_sweep_wait(start.elapsed());
                    resting
                }
                Err(mpsc::TryRecvError::Disconnected) => None,
            },
        }
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
    /// Waits for a sweep the previous job started if it is still running.
    pub fn take(&self, ordered_threshold: usize) -> RestingPredicates {
        let stored = self.members.lock().take();
        stored
            .and_then(|(finishing, block_hash)| {
                let mut resting = finishing.wait()?;
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
        if !resting.sweep_due() || resting.is_empty() {
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
        state::{Account, AccountInfo},
    };

    use super::*;

    const WATCHED: Address = Address::repeat_byte(0xaa);
    const UNRELATED: Address = Address::repeat_byte(0xbb);
    const PARENT: B256 = B256::repeat_byte(0x01);
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
            let mut resting = store.take(32);
            let carry_over = resting.begin_block(parent, block, Some(&TransitionState::default()));
            assert!(!carry_over.reset, "block {block} must follow the finished block");
            if block <= held_through {
                assert_eq!(resting.hold(hash(0), held), Some(true));
            }
            parent = B256::with_last_byte(block as u8);
            store.put(store.finish(resting), parent);
        }
        store.take(32)
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

        let mut resting = store.take(32);
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
        let mut resting = store.take(32);
        resting.begin_block(B256::ZERO, 1, Some(&TransitionState::default()));
        resting.rest(hash(0), balance_at_least(WATCHED, 1));
        store.put(store.finish(resting), PARENT);

        assert_eq!(store.take(32).len(), 1);
        assert!(store.take(32).is_empty(), "a taken set leaves the store empty");
    }
}
