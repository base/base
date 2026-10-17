//! Per-block fill-immediately-or-kill decisions derived from an in-block predicate write journal.
//!
//! A validity transaction flagged
//! [`fill_immediately_or_kill`](base_execution_txpool::TransactionValidity::fill_immediately_or_kill)
//! asks to execute against the exact predicate state it was first satisfied on, or not at all. The
//! builder decides this from two per-block facts, without executing or rolling back anything:
//!
//! * the [`PredicateWriteJournal`], a history of every in-block write to a location a validity
//!   predicate can read (balances, nonces, storage slots), and
//! * whether the flagged transaction's predicates held at an earlier point in the block.
//!
//! Given a flagged transaction whose predicates hold now, the journal replays the block's writes to
//! the locations those predicates read and finds the earliest position at which the predicates
//! held. If any of those locations was written after that position, a competitor or an ordinary
//! transaction moved the state the transaction conditioned on, and it is killed. The decision is a
//! function of the block's transaction sequence and the predicates alone: it does not depend on
//! which flashblock first considered the transaction or on the priority order of the scan.
//!
//! A flagged transaction whose predicates held in the block but which the block closed without
//! including is killed as well: "immediately" means within the block in which the predicates
//! first hold.

use std::collections::BTreeSet;

use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{B256Map, B256Set, HashMap},
};
use base_execution_txpool::{PredicateContext, ValidityPredicate};
use revm::{
    Database,
    state::{AccountInfo, Bytecode, EvmState},
};

use crate::ValidityPredicateKey;

/// Why a fill-immediately-or-kill transaction was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// A location its predicates read was written after they first held in this block.
    StateOverwritten,
    /// Its predicates held in this block, but the block closed without including it.
    Unfilled,
}

impl KillReason {
    /// Stable decision-event reason label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::StateOverwritten => "fill_immediately_or_kill_state_overwritten",
            Self::Unfilled => "fill_immediately_or_kill_unfilled",
        }
    }

    /// Human-readable decision-event detail.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::StateOverwritten => {
                "state read by the transaction's predicates was written after they first held in this block"
            }
            Self::Unfilled => {
                "the transaction's predicates held in this block but it was not included before the block closed"
            }
        }
    }
}

/// A fill-immediately-or-kill transaction dropped by the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kill {
    /// Hash of the dropped transaction.
    pub tx_hash: TxHash,
    /// Ordering position (or validity consideration index) the transaction was last reported at.
    pub ordering_position: u64,
    /// Why it was dropped.
    pub reason: KillReason,
}

/// Where a satisfied fill-immediately-or-kill transaction stands against the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedState {
    /// No location the predicates read was written since they first held: the transaction may
    /// execute against the state it conditioned on.
    Intact,
    /// A location the predicates read was written after they first held.
    Overwritten {
        /// Number of committed transactions after which the predicates first held.
        satisfied_at: u32,
        /// Index of the committed transaction that first wrote a read location afterwards.
        overwritten_by: u32,
    },
}

/// In-block write history of one predicate location.
#[derive(Debug)]
struct LocationHistory {
    /// Value at the start of the block, before any pool transaction committed.
    original: U256,
    /// Writes as (index of the committing transaction, value after it), in commit order.
    writes: Vec<(u32, U256)>,
}

impl LocationHistory {
    /// Value of the location after `position` transactions have committed.
    fn value_at(&self, position: u32) -> U256 {
        self.writes
            .iter()
            .rev()
            .find(|(index, _)| *index < position)
            .map_or(self.original, |(_, value)| *value)
    }
}

/// History of in-block writes to the state that validity predicates can read.
///
/// Positions count committed pool transactions: position `0` is the state the block's pool
/// transactions start from (after sequencer transactions), and position `n` is the state after
/// the first `n` committed pool transactions. Each write is attributed to the index of the
/// transaction that committed it, so the state at position `n` reflects writes by transactions
/// `0..n`.
#[derive(Debug, Default)]
pub struct PredicateWriteJournal {
    histories: HashMap<ValidityPredicateKey, LocationHistory>,
    committed: u32,
    /// Position at which each flashblock started building pool transactions, with its index.
    flashblock_starts: Vec<(u32, u64)>,
}

impl PredicateWriteJournal {
    /// Marks that pool transactions committed from now on belong to flashblock `index`.
    pub fn begin_flashblock(&mut self, index: u64) {
        self.flashblock_starts.push((self.committed, index));
    }

    /// Records the state changes of one committed transaction and advances the position.
    pub fn record(&mut self, state: &EvmState) {
        let position = self.committed;
        for (address, account) in state {
            let original = account.original_info();
            // Deleted accounts read as nonce zero and balance zero, whatever info they retain.
            let (nonce, balance) = if account.is_selfdestructed() {
                (0, U256::ZERO)
            } else {
                (account.info.nonce, account.info.balance)
            };
            if nonce != original.nonce {
                self.write(
                    ValidityPredicateKey::Nonce(*address),
                    U256::from(original.nonce),
                    U256::from(nonce),
                    position,
                );
            }
            if balance != original.balance {
                self.write(
                    ValidityPredicateKey::Balance(*address),
                    original.balance,
                    balance,
                    position,
                );
            }
            if account.is_selfdestructed() {
                // EIP-6780 limits destruction to the creating transaction, so the account had no
                // storage at block start and reads as empty afterwards: nothing to record.
                continue;
            }
            for (slot, value) in &account.storage {
                if value.is_changed() {
                    self.write(
                        ValidityPredicateKey::Storage(*address, *slot),
                        value.original_value(),
                        value.present_value(),
                        position,
                    );
                }
            }
        }
        self.committed += 1;
    }

    fn write(&mut self, key: ValidityPredicateKey, original: U256, value: U256, position: u32) {
        self.histories
            .entry(key)
            .or_insert_with(|| LocationHistory { original, writes: Vec::new() })
            .writes
            .push((position, value));
    }

    /// Number of committed transactions recorded so far.
    pub const fn committed(&self) -> u32 {
        self.committed
    }

    /// Checks a transaction whose `predicates` hold at the current position against the block's
    /// write history.
    ///
    /// Replays the block at every position where the predicates' truth could have changed (the
    /// block start, each flashblock start, and the state after each write to a read location) and
    /// finds the earliest one at which they held. The transaction is [`PinnedState::Overwritten`]
    /// if a read location was written at or after that position.
    pub fn pinned_state<DB: Database>(
        &self,
        predicates: &[ValidityPredicate],
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<PinnedState, DB::Error> {
        let mut candidates = BTreeSet::from([0_u32]);
        let mut last_write = None;
        for predicate in predicates {
            let Some(history) = self.histories.get(&ValidityPredicateKey::for_predicate(predicate))
            else {
                continue;
            };
            for (index, _) in &history.writes {
                candidates.insert(index + 1);
                last_write = last_write.max(Some(*index));
            }
        }
        let Some(last_write) = last_write else {
            return Ok(PinnedState::Intact);
        };
        candidates.extend(self.flashblock_starts.iter().map(|(position, _)| *position));

        for position in candidates {
            if position > last_write {
                break;
            }
            let historical_context = PredicateContext {
                block_number: context.block_number,
                flashblock_index: self.flashblock_at(position).unwrap_or(context.flashblock_index),
            };
            let mut view = HistoricalView { journal: self, position, db: &mut *db };
            if ValidityPredicateKey::first_unsatisfied(predicates, &mut view, &historical_context)?
                .is_none()
            {
                return Ok(PinnedState::Overwritten {
                    satisfied_at: position,
                    overwritten_by: self
                        .first_write_at_or_after(predicates, position)
                        .expect("a write at or after the satisfied position exists"),
                });
            }
        }
        Ok(PinnedState::Intact)
    }

    /// Index of the flashblock whose pool transactions were being committed at `position`.
    fn flashblock_at(&self, position: u32) -> Option<u64> {
        self.flashblock_starts
            .iter()
            .rev()
            .find(|(start, _)| *start <= position)
            .map(|(_, index)| *index)
    }

    fn first_write_at_or_after(
        &self,
        predicates: &[ValidityPredicate],
        position: u32,
    ) -> Option<u32> {
        predicates
            .iter()
            .filter_map(|predicate| {
                self.histories.get(&ValidityPredicateKey::for_predicate(predicate))
            })
            .flat_map(|history| history.writes.iter().map(|(index, _)| *index))
            .filter(|index| *index >= position)
            .min()
    }

    fn value_at(&self, key: ValidityPredicateKey, position: u32) -> Option<U256> {
        self.histories.get(&key).map(|history| history.value_at(position))
    }
}

/// A read-only [`Database`] view of the block as of a journal position.
///
/// Locations with journal history read their historical value; everything else was not written
/// in the block, so its current value is also its value at every position.
struct HistoricalView<'a, DB> {
    journal: &'a PredicateWriteJournal,
    position: u32,
    db: &'a mut DB,
}

impl<DB: Database> Database for HistoricalView<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let balance = self.journal.value_at(ValidityPredicateKey::Balance(address), self.position);
        let nonce = self.journal.value_at(ValidityPredicateKey::Nonce(address), self.position);
        if balance.is_none() && nonce.is_none() {
            return self.db.basic(address);
        }
        let mut info = self.db.basic(address)?.unwrap_or_default();
        if let Some(balance) = balance {
            info.balance = balance;
        }
        if let Some(nonce) = nonce {
            info.nonce = nonce.saturating_to();
        }
        Ok(Some(info))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.journal.value_at(ValidityPredicateKey::Storage(address, index), self.position) {
            Some(value) => Ok(value),
            None => self.db.storage(address, index),
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

/// Per-block fill-immediately-or-kill bookkeeping for one builder.
///
/// Feed every committed pool transaction's state through [`Self::record_committed_state`], ask
/// [`Self::check_satisfied`] before executing a flagged transaction whose predicates hold, report
/// inclusions with [`Self::record_included`], and drain the block's kills with [`Self::finish`]
/// once the block closes. Create one per block build.
#[derive(Debug, Default)]
pub struct FillOrKillTracker {
    journal: PredicateWriteJournal,
    /// Flagged transactions whose predicates held in this block and which are neither included
    /// nor killed yet, with the ordering position they were last reported at.
    satisfied: B256Map<u64>,
    killed: Vec<Kill>,
    killed_hashes: B256Set,
}

impl FillOrKillTracker {
    /// Marks that pool transactions committed from now on belong to flashblock `index`.
    pub fn begin_flashblock(&mut self, index: u64) {
        self.journal.begin_flashblock(index);
    }

    /// Records the state changes of one committed pool transaction.
    pub fn record_committed_state(&mut self, state: &EvmState) {
        self.journal.record(state);
    }

    /// Returns whether `tx_hash` was already killed in this block.
    pub fn is_killed(&self, tx_hash: &TxHash) -> bool {
        self.killed_hashes.contains(tx_hash)
    }

    /// Decides whether a flagged transaction whose `predicates` hold now may execute.
    ///
    /// Returns the kill when a location the predicates read was written after they first held
    /// in this block. Otherwise records the satisfaction so that [`Self::finish`] can kill the
    /// transaction if the block closes without it.
    pub fn check_satisfied<DB: Database>(
        &mut self,
        tx_hash: TxHash,
        ordering_position: u64,
        predicates: &[ValidityPredicate],
        db: &mut DB,
        context: &PredicateContext,
    ) -> Result<Option<Kill>, DB::Error> {
        if self.journal.pinned_state(predicates, db, context)? == PinnedState::Intact {
            self.satisfied.insert(tx_hash, ordering_position);
            return Ok(None);
        }
        self.satisfied.remove(&tx_hash);
        let kill = Kill { tx_hash, ordering_position, reason: KillReason::StateOverwritten };
        self.killed.push(kill);
        self.killed_hashes.insert(tx_hash);
        Ok(Some(kill))
    }

    /// Records that a flagged transaction was included.
    pub fn record_included(&mut self, tx_hash: &TxHash) {
        self.satisfied.remove(tx_hash);
    }

    /// Closes the block: every satisfied transaction that was not included is killed as
    /// [`KillReason::Unfilled`]. Returns all kills of the block, in-block ones first.
    pub fn finish(&mut self) -> Vec<Kill> {
        let mut unfilled = std::mem::take(&mut self.satisfied)
            .into_iter()
            .map(|(tx_hash, ordering_position)| Kill {
                tx_hash,
                ordering_position,
                reason: KillReason::Unfilled,
            })
            .collect::<Vec<_>>();
        // Hash-map order is arbitrary; report in a stable order.
        unfilled.sort_by_key(|kill| (kill.ordering_position, kill.tx_hash));
        self.killed_hashes.extend(unfilled.iter().map(|kill| kill.tx_hash));
        let mut killed = std::mem::take(&mut self.killed);
        killed.extend(unfilled);
        killed
    }
}

#[cfg(test)]
mod tests {
    use base_execution_txpool::ValidityOperator;
    use revm::{
        database::InMemoryDB,
        state::{Account, EvmStorageSlot, TransactionId},
    };

    use super::*;

    const POOL: Address = Address::repeat_byte(0x11);
    const VICTIM: Address = Address::repeat_byte(0x22);

    fn slot(slot: u64, op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address: POOL,
            slot: U256::from(slot),
            mask: U256::MAX,
            op,
            value: U256::from(value),
        }
    }

    fn nonce_at_least(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Nonce {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn flashblock_at_least(value: u64) -> ValidityPredicate {
        ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn context(flashblock_index: u64) -> PredicateContext {
        PredicateContext { block_number: 100, flashblock_index }
    }

    /// A committed transaction that wrote `slot` of the pool from `old` to `new`.
    fn pool_write(slot: u64, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.mark_touch();
        account.storage.insert(
            U256::from(slot),
            EvmStorageSlot::new_changed(
                U256::from(old),
                U256::from(new),
                TransactionId::new(0).expect("zero is a valid transaction id"),
            ),
        );
        EvmState::from_iter([(POOL, account)])
    }

    /// A committed transaction that advanced `address`'s nonce from `old` to `new`.
    fn nonce_write(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.mark_touch();
        account.original_info_mut().nonce = old;
        account.info.nonce = new;
        EvmState::from_iter([(address, account)])
    }

    /// A database whose pool slot and victim nonce read the block's final values.
    fn db(slot_value: u64, victim_nonce: u64) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            VICTIM,
            AccountInfo { nonce: victim_nonce, ..AccountInfo::default() },
        );
        db.insert_account_storage(POOL, U256::from(1), U256::from(slot_value)).unwrap();
        db
    }

    #[test]
    fn untouched_predicate_state_is_intact() {
        let mut journal = PredicateWriteJournal::default();
        journal.record(&pool_write(2, 0, 5));
        let predicates = [slot(1, ValidityOperator::LessThan, 100)];

        let pinned = journal.pinned_state(&predicates, &mut db(10, 0), &context(1)).unwrap();

        assert_eq!(pinned, PinnedState::Intact);
    }

    #[test]
    fn write_after_block_start_satisfaction_overwrites() {
        // Loose predicate that held at block start; a competitor moved the slot afterwards.
        let mut journal = PredicateWriteJournal::default();
        journal.record(&pool_write(1, 10, 20));
        let predicates = [slot(1, ValidityOperator::LessThan, 100)];

        let pinned = journal.pinned_state(&predicates, &mut db(20, 0), &context(1)).unwrap();

        assert_eq!(pinned, PinnedState::Overwritten { satisfied_at: 0, overwritten_by: 0 });
    }

    #[test]
    fn the_write_that_satisfies_the_predicates_is_not_a_conflict() {
        // A backrun: the predicate is false until the trigger writes the slot.
        let mut journal = PredicateWriteJournal::default();
        journal.record(&pool_write(1, 10, 20));
        let predicates = [slot(1, ValidityOperator::GreaterThanOrEqual, 20)];

        let pinned = journal.pinned_state(&predicates, &mut db(20, 0), &context(1)).unwrap();

        assert_eq!(pinned, PinnedState::Intact);
    }

    #[test]
    fn a_second_write_after_the_trigger_overwrites() {
        // Trigger makes the predicates true at position 1; a competitor writes at index 2 while
        // the loose predicate keeps holding.
        let mut journal = PredicateWriteJournal::default();
        journal.record(&nonce_write(Address::repeat_byte(0x33), 0, 1));
        journal.record(&pool_write(1, 10, 20));
        journal.record(&pool_write(1, 20, 25));
        let predicates = [slot(1, ValidityOperator::GreaterThanOrEqual, 20)];

        let pinned = journal.pinned_state(&predicates, &mut db(25, 0), &context(1)).unwrap();

        assert_eq!(pinned, PinnedState::Overwritten { satisfied_at: 2, overwritten_by: 2 });
    }

    #[test]
    fn predicates_over_several_locations_pin_all_of_them() {
        // Satisfied once the victim's nonce advances; the pool write afterwards overwrites even
        // though the pool predicate alone would still hold.
        let mut journal = PredicateWriteJournal::default();
        journal.record(&nonce_write(VICTIM, 4, 5));
        journal.record(&pool_write(1, 10, 11));
        let predicates = [nonce_at_least(VICTIM, 5), slot(1, ValidityOperator::LessThan, 100)];

        let pinned = journal.pinned_state(&predicates, &mut db(11, 5), &context(1)).unwrap();

        assert_eq!(pinned, PinnedState::Overwritten { satisfied_at: 1, overwritten_by: 1 });
    }

    #[test]
    fn a_write_that_does_not_change_the_value_is_not_recorded() {
        let mut journal = PredicateWriteJournal::default();
        journal.record(&pool_write(1, 10, 10));
        let predicates = [slot(1, ValidityOperator::LessThan, 100)];

        assert_eq!(
            journal.pinned_state(&predicates, &mut db(10, 0), &context(1)).unwrap(),
            PinnedState::Intact
        );
    }

    #[test]
    fn flashblock_bound_delays_satisfaction_until_the_flashblock_starts() {
        // The slot held from the start, but the transaction could only fill from flashblock 2.
        // A write in flashblock 1 is therefore not a conflict; a write in flashblock 2 is.
        let mut journal = PredicateWriteJournal::default();
        journal.begin_flashblock(1);
        journal.record(&pool_write(1, 10, 11));
        journal.begin_flashblock(2);
        let predicates = [flashblock_at_least(2), slot(1, ValidityOperator::LessThan, 100)];
        assert_eq!(
            journal.pinned_state(&predicates, &mut db(11, 0), &context(2)).unwrap(),
            PinnedState::Intact
        );

        journal.record(&pool_write(1, 11, 12));
        assert_eq!(
            journal.pinned_state(&predicates, &mut db(12, 0), &context(2)).unwrap(),
            PinnedState::Overwritten { satisfied_at: 1, overwritten_by: 1 }
        );
    }

    #[test]
    fn tracker_kills_satisfied_transactions_the_block_closes_without() {
        let mut tracker = FillOrKillTracker::default();
        let filled = TxHash::repeat_byte(1);
        let unfilled = TxHash::repeat_byte(2);
        let killed = TxHash::repeat_byte(3);
        let predicates = [slot(1, ValidityOperator::LessThan, 100)];
        let mut db = db(10, 0);

        assert_eq!(
            tracker.check_satisfied(filled, 1, &predicates, &mut db, &context(1)).unwrap(),
            None
        );
        tracker.record_included(&filled);
        assert_eq!(
            tracker.check_satisfied(unfilled, 2, &predicates, &mut db, &context(1)).unwrap(),
            None
        );
        tracker.record_committed_state(&pool_write(1, 10, 11));
        let kill = tracker.check_satisfied(killed, 3, &predicates, &mut db, &context(1)).unwrap();
        assert_eq!(
            kill,
            Some(Kill {
                tx_hash: killed,
                ordering_position: 3,
                reason: KillReason::StateOverwritten
            })
        );
        assert!(tracker.is_killed(&killed));
        assert!(!tracker.is_killed(&unfilled));

        let kills = tracker.finish();

        assert_eq!(
            kills,
            vec![
                Kill {
                    tx_hash: killed,
                    ordering_position: 3,
                    reason: KillReason::StateOverwritten
                },
                Kill { tx_hash: unfilled, ordering_position: 2, reason: KillReason::Unfilled },
            ]
        );
        assert!(tracker.is_killed(&unfilled));
        assert!(tracker.finish().is_empty());
    }
}
