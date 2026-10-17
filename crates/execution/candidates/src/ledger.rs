//! Group ledger (design §4.4): one atomic record per `groupHash`, independent of pool membership.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, B256, TxHash};
use parking_lot::Mutex;

use crate::{CandidateError, CandidateGroup, PrincipalId};

/// Default number of blocks a ledger entry and its tombstones outlive `expiryBlock` (design Q16).
pub const DEFAULT_RECOVERY_WINDOW_BLOCKS: u64 = 64;

/// Default number of live groups one sender may hold.
pub const DEFAULT_MAX_GROUPS_PER_SENDER: usize = 1;

/// Default number of live groups the pool may hold.
pub const DEFAULT_MAX_GROUPS: usize = 1_024;

/// Default server cap on evaluation rounds per group, below any signed `maxEvaluations`.
pub const DEFAULT_MAX_EVALUATIONS: u16 = 8;

/// Why a group left the pool without landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionReason {
    /// The target block passed `expiryBlock`.
    Expired,
    /// `DROP` group with no budget left.
    BudgetExhausted,
    /// The principal is no longer authorized.
    Revoked,
    /// Fallback hit a transaction-scope permanent exclusion.
    PermanentExclusion,
    /// The representative left the pool by another path (replacement, admin drop, discard).
    Dropped,
}

impl EvictionReason {
    /// Returns a stable metric and event label.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Revoked => "revoked",
            Self::PermanentExclusion => "permanent_exclusion",
            Self::Dropped => "dropped",
        }
    }
}

/// Lifecycle state of a ledger entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupState {
    /// Created at ingress; pool insertion is in flight.
    Pending,
    /// The representative is pooled.
    Pooled,
    /// A sibling landed in a canonical block.
    Landed {
        /// The sibling that landed.
        winner: TxHash,
        /// Block the builder committed it in.
        block: u64,
    },
    /// Replaced by a different group or a plain same-nonce transaction.
    Replaced,
    /// Left the pool without landing.
    Evicted(EvictionReason),
}

impl GroupState {
    /// Returns `true` for states that end the group: resubmission returns `group_terminal`.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Landed { .. } | Self::Replaced | Self::Evicted(_))
    }
}

/// A snapshot of one ledger entry.
#[derive(Debug, Clone)]
pub struct GroupLedgerEntry {
    /// The complete signed group, retained for recovery decisions after it leaves the pool.
    pub group: Arc<CandidateGroup>,
    /// Principal bound at admission.
    pub principal: PrincipalId,
    /// Registry version bound at admission.
    pub policy_version: u64,
    /// Lifecycle state.
    pub state: GroupState,
    /// Evaluation rounds started; monotonic.
    pub attempts: u16,
    /// Work gas reserved by rounds that have not settled.
    pub work_gas_reserved: u64,
    /// Work gas settled by finished rounds.
    pub work_gas_settled: u64,
    /// Siblings excluded by a transaction-scope permanent error; never trialed again.
    pub permanently_ineligible: Vec<bool>,
    /// The latest sibling the builder committed for this group, and the block.
    pub last_commit: Option<(TxHash, u64)>,
}

impl GroupLedgerEntry {
    /// Returns signed `maxWorkGas` minus reserved and settled work.
    pub fn remaining_work_gas(&self) -> u64 {
        self.group
            .envelope()
            .max_work_gas
            .saturating_sub(self.work_gas_reserved)
            .saturating_sub(self.work_gas_settled)
    }

    /// Returns `true` when the group-owned budget cannot cover one more round.
    pub fn budget_exhausted(&self, round_gas: u64, server_max_evaluations: u16) -> bool {
        let max_evaluations = self.group.envelope().max_evaluations.min(server_max_evaluations);
        self.attempts >= max_evaluations || self.remaining_work_gas() < round_gas
    }

    const fn is_live(&self) -> bool {
        matches!(self.state, GroupState::Pending | GroupState::Pooled)
    }
}

/// Result of [`GroupLedger::create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerAdmission {
    /// A new entry was created; insert the group into the pool, then confirm or abort.
    Created,
    /// The same `groupHash` is already live; return the original response. No slot is
    /// reserved and no budget is refreshed.
    Duplicate,
}

/// Group work reserved for one round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupReservation {
    /// Group identity.
    pub group_hash: B256,
    /// The attempt number this round consumed.
    pub attempt: u16,
    /// Work gas reserved.
    pub gas: u64,
}

/// Why a round could not reserve group work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReserveError {
    /// The ledger has no live entry for the group.
    #[error("unknown or terminal candidate group")]
    Unknown,
    /// The group-owned budget cannot cover the round.
    #[error("candidate group budget exhausted")]
    Exhausted,
}

/// Capacity and retention settings for the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerLimits {
    /// Live groups one sender may hold, counting other nonces only (same-nonce is replacement).
    pub max_groups_per_sender: usize,
    /// Live groups the pool may hold.
    pub max_groups: usize,
    /// Blocks an entry outlives its `expiryBlock`.
    pub recovery_window_blocks: u64,
}

impl Default for LedgerLimits {
    fn default() -> Self {
        Self {
            max_groups_per_sender: DEFAULT_MAX_GROUPS_PER_SENDER,
            max_groups: DEFAULT_MAX_GROUPS,
            recovery_window_blocks: DEFAULT_RECOVERY_WINDOW_BLOCKS,
        }
    }
}

#[derive(Debug, Default)]
struct LedgerState {
    entries: HashMap<B256, GroupLedgerEntry>,
    /// Sibling hash to the latest group that contained it.
    tombstones: HashMap<TxHash, B256>,
}

/// In-memory group ledger. Every operation is atomic under one lock.
///
/// Entries live until `expiryBlock + recovery_window_blocks`, whether or not the group is
/// pooled, so duplicates, budgets and sibling tombstones survive pool removal.
#[derive(Debug, Default)]
pub struct GroupLedger {
    limits: LedgerLimits,
    state: Mutex<LedgerState>,
}

impl GroupLedger {
    /// Creates an empty ledger.
    pub fn new(limits: LedgerLimits) -> Self {
        Self { limits, state: Mutex::default() }
    }

    /// Creates an entry for a validated group, or classifies it as a duplicate.
    ///
    /// `is_pooled` reports whether a representative hash is still in the pool; it lets the
    /// ledger release capacity held by groups the pool dropped through paths that do not notify
    /// it.
    pub fn create(
        &self,
        group: Arc<CandidateGroup>,
        principal: PrincipalId,
        policy_version: u64,
        is_pooled: impl Fn(&TxHash) -> bool,
    ) -> Result<LedgerAdmission, CandidateError> {
        let mut state = self.state.lock();
        Self::reconcile_locked(&mut state, &is_pooled);
        let group_hash = group.group_hash();
        if let Some(existing) = state.entries.get(&group_hash) {
            return if existing.state.is_terminal() {
                Err(CandidateError::GroupTerminal)
            } else {
                Ok(LedgerAdmission::Duplicate)
            };
        }
        let representative = group.representative().hash();
        if let Some(other) = state.tombstones.get(&representative)
            && state.entries.get(other).is_some_and(GroupLedgerEntry::is_live)
        {
            return Err(CandidateError::RepresentativeConflict);
        }
        let live = state.entries.values().filter(|entry| entry.is_live());
        let (mut sender_groups, mut total) = (0_usize, 0_usize);
        for entry in live {
            total += 1;
            if entry.group.sender() == group.sender() && entry.group.nonce() != group.nonce() {
                sender_groups += 1;
            }
        }
        if sender_groups >= self.limits.max_groups_per_sender {
            return Err(CandidateError::SenderGroupLimit);
        }
        if total >= self.limits.max_groups {
            return Err(CandidateError::PoolGroupLimit);
        }
        for sibling in group.siblings() {
            state.tombstones.insert(sibling.hash(), group_hash);
        }
        let siblings = group.siblings().len();
        state.entries.insert(
            group_hash,
            GroupLedgerEntry {
                group,
                principal,
                policy_version,
                state: GroupState::Pending,
                attempts: 0,
                work_gas_reserved: 0,
                work_gas_settled: 0,
                permanently_ineligible: vec![false; siblings],
                last_commit: None,
            },
        );
        Ok(LedgerAdmission::Created)
    }

    /// Marks a created group pooled. Any other live group of the same sender and nonce was
    /// replaced by it and becomes terminal, keeping its counters.
    pub fn confirm_pooled(&self, group_hash: B256) {
        let mut state = self.state.lock();
        let Some(entry) = state.entries.get(&group_hash) else { return };
        let (sender, nonce) = (entry.group.sender(), entry.group.nonce());
        for (hash, other) in &mut state.entries {
            if *hash != group_hash
                && other.is_live()
                && other.group.sender() == sender
                && other.group.nonce() == nonce
            {
                other.state = GroupState::Replaced;
            }
        }
        if let Some(entry) = state.entries.get_mut(&group_hash) {
            entry.state = GroupState::Pooled;
        }
    }

    /// Removes a created group whose pool insertion failed. Nothing was spent.
    pub fn abort(&self, group_hash: B256) {
        let mut state = self.state.lock();
        if state.entries.get(&group_hash).is_some_and(|entry| entry.state == GroupState::Pending) {
            state.entries.remove(&group_hash);
            state.tombstones.retain(|_, group| *group != group_hash);
        }
    }

    /// Returns a snapshot of the entry for `group_hash`.
    pub fn lookup(&self, group_hash: &B256) -> Option<GroupLedgerEntry> {
        self.state.lock().entries.get(group_hash).cloned()
    }

    /// Returns the group a sibling hash belongs to, if the ledger still retains it.
    pub fn group_of_sibling(&self, hash: &TxHash) -> Option<B256> {
        self.state.lock().tombstones.get(hash).copied()
    }

    /// Returns `true` when an automatic reinsertion of `hash` as a plain transaction must be
    /// suppressed because it is a known candidate sibling (design §4.6).
    pub fn suppresses_plain_reinsertion(&self, hash: &TxHash) -> bool {
        self.state.lock().tombstones.contains_key(hash)
    }

    /// Atomically consumes an attempt and reserves `gas` of group work for one round.
    pub fn reserve(
        &self,
        group_hash: B256,
        gas: u64,
        server_max_evaluations: u16,
    ) -> Result<GroupReservation, ReserveError> {
        let mut state = self.state.lock();
        let entry = state
            .entries
            .get_mut(&group_hash)
            .filter(|entry| entry.is_live())
            .ok_or(ReserveError::Unknown)?;
        if entry.budget_exhausted(gas, server_max_evaluations) {
            return Err(ReserveError::Exhausted);
        }
        entry.attempts += 1;
        entry.work_gas_reserved = entry.work_gas_reserved.saturating_add(gas);
        Ok(GroupReservation { group_hash, attempt: entry.attempts, gas })
    }

    /// Settles a reservation at actual work and releases the unused part.
    pub fn settle(&self, reservation: GroupReservation, gas_used: u64) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(&reservation.group_hash) {
            entry.work_gas_reserved = entry.work_gas_reserved.saturating_sub(reservation.gas);
            entry.work_gas_settled =
                entry.work_gas_settled.saturating_add(gas_used.min(reservation.gas));
        }
    }

    /// Records a transaction-scope permanent exclusion for one sibling.
    pub fn mark_permanently_ineligible(&self, group_hash: B256, index: usize) {
        if let Some(flag) = self
            .state
            .lock()
            .entries
            .get_mut(&group_hash)
            .and_then(|entry| entry.permanently_ineligible.get_mut(index))
        {
            *flag = true;
        }
    }

    /// Records that the builder committed `winner` for the group in `block`.
    ///
    /// The commit is provisional until the block is canonical; the entry becomes `Landed` when
    /// the representative leaves the pool (see [`Self::reconcile`]).
    pub fn record_commit(&self, group_hash: B256, winner: TxHash, block: u64) {
        if let Some(entry) = self.state.lock().entries.get_mut(&group_hash) {
            entry.last_commit = Some((winner, block));
        }
    }

    /// Moves a live group to a terminal state.
    pub fn evict(&self, group_hash: B256, reason: EvictionReason) {
        if let Some(entry) = self.state.lock().entries.get_mut(&group_hash)
            && entry.is_live()
        {
            entry.state = GroupState::Evicted(reason);
        }
    }

    /// Settles pooled entries whose representative is no longer pooled: `Landed` when the
    /// builder committed a sibling, otherwise `Evicted(Dropped)`.
    pub fn reconcile(&self, is_pooled: impl Fn(&TxHash) -> bool) {
        Self::reconcile_locked(&mut self.state.lock(), &is_pooled);
    }

    /// Drops entries and tombstones whose recovery window ended before `committed_block`.
    pub fn prune(&self, committed_block: u64) {
        let window = self.limits.recovery_window_blocks;
        let mut state = self.state.lock();
        state.entries.retain(|_, entry| {
            entry.group.envelope().expiry_block.saturating_add(window) >= committed_block
        });
        let LedgerState { entries, tombstones } = &mut *state;
        tombstones.retain(|_, group| entries.contains_key(group));
    }

    /// Returns the number of live groups and the total number of retained entries.
    pub fn counts(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.entries.values().filter(|entry| entry.is_live()).count(), state.entries.len())
    }

    /// Returns the live group currently held by `sender` at `nonce`, if any.
    pub fn live_group(&self, sender: Address, nonce: u64) -> Option<B256> {
        self.state.lock().entries.iter().find_map(|(hash, entry)| {
            (entry.is_live() && entry.group.sender() == sender && entry.group.nonce() == nonce)
                .then_some(*hash)
        })
    }

    fn reconcile_locked(state: &mut LedgerState, is_pooled: &impl Fn(&TxHash) -> bool) {
        for entry in state.entries.values_mut() {
            if entry.state == GroupState::Pooled && !is_pooled(&entry.group.representative().hash())
            {
                entry.state = match entry.last_commit {
                    Some((winner, block)) => GroupState::Landed { winner, block },
                    None => GroupState::Evicted(EvictionReason::Dropped),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::*;
    use crate::test_utils::CandidateFixture;

    const CHAIN_ID: u64 = 8453;
    const PRINCIPAL: PrincipalId = PrincipalId(1);

    fn group(
        fixture: &CandidateFixture,
        nonce: u64,
        seed: u8,
        default_index: u8,
    ) -> Arc<CandidateGroup> {
        let raw: Vec<_> = (0..2)
            .map(|index| {
                fixture.encode(fixture.sibling_tx(
                    nonce,
                    Address::repeat_byte(seed + index),
                    U256::ZERO,
                ))
            })
            .collect();
        let mut envelope = fixture.envelope_for(&raw, default_index);
        envelope.nonce = nonce;
        Arc::new(fixture.group(&raw, envelope))
    }

    fn pooled(_: &TxHash) -> bool {
        true
    }

    #[test]
    fn exact_duplicate_does_not_reserve_or_refresh() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled),
            Ok(LedgerAdmission::Created)
        );
        ledger.confirm_pooled(group.group_hash());
        ledger.reserve(group.group_hash(), 10, 8).unwrap();
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled),
            Ok(LedgerAdmission::Duplicate)
        );
        let entry = ledger.lookup(&group.group_hash()).unwrap();
        assert_eq!((entry.attempts, entry.work_gas_reserved), (1, 10));
        assert_eq!(ledger.counts(), (1, 1));
    }

    #[test]
    fn terminal_group_is_rejected_on_resubmission() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(group.group_hash());
        ledger.evict(group.group_hash(), EvictionReason::BudgetExhausted);
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled),
            Err(CandidateError::GroupTerminal)
        );
    }

    #[test]
    fn same_representative_under_different_envelope_conflicts() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let first = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&first), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(first.group_hash());

        let raw: Vec<_> = first.siblings().iter().map(|sibling| sibling.encoded.clone()).collect();
        let mut envelope = first.envelope().clone();
        envelope.max_evaluations = 5;
        let second = Arc::new(fixture.group(&raw, envelope));
        assert_eq!(
            ledger.create(second, PRINCIPAL, 1, pooled),
            Err(CandidateError::RepresentativeConflict)
        );
    }

    #[test]
    fn sender_limit_counts_other_nonces_and_allows_replacement() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let first = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&first), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(first.group_hash());

        let other_nonce = group(&fixture, 1, 10, 0);
        assert_eq!(
            ledger.create(other_nonce, PRINCIPAL, 1, pooled),
            Err(CandidateError::SenderGroupLimit)
        );

        let replacement = group(&fixture, 0, 20, 0);
        ledger.create(Arc::clone(&replacement), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(replacement.group_hash());
        assert_eq!(ledger.lookup(&first.group_hash()).unwrap().state, GroupState::Replaced);
        assert_eq!(ledger.live_group(fixture.address(), 0), Some(replacement.group_hash()));
    }

    #[test]
    fn dropped_groups_release_capacity_lazily() {
        let ledger = GroupLedger::new(LedgerLimits { max_groups: 1, ..Default::default() });
        let fixture = CandidateFixture::new(CHAIN_ID);
        let other = CandidateFixture::with_key(CHAIN_ID, 0x55);
        let first = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&first), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(first.group_hash());
        assert_eq!(
            ledger.create(group(&other, 0, 1, 0), PRINCIPAL, 1, pooled),
            Err(CandidateError::PoolGroupLimit)
        );
        ledger.create(group(&other, 0, 1, 0), PRINCIPAL, 1, |_| false).unwrap();
        assert_eq!(
            ledger.lookup(&first.group_hash()).unwrap().state,
            GroupState::Evicted(EvictionReason::Dropped)
        );
    }

    #[test]
    fn reconcile_marks_committed_group_landed() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(group.group_hash());
        let winner = group.siblings()[1].hash();
        ledger.record_commit(group.group_hash(), winner, 12);
        ledger.reconcile(|_| false);
        assert_eq!(
            ledger.lookup(&group.group_hash()).unwrap().state,
            GroupState::Landed { winner, block: 12 }
        );
    }

    #[test]
    fn abort_removes_pending_entry_and_tombstones() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled).unwrap();
        ledger.abort(group.group_hash());
        assert!(ledger.lookup(&group.group_hash()).is_none());
        assert!(!ledger.suppresses_plain_reinsertion(&group.siblings()[1].hash()));
    }

    #[test]
    fn reservations_enforce_attempts_and_work_and_settle_refunds() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        let hash = group.group_hash();
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(hash);

        // Signed maxEvaluations = 3, maxWorkGas = 10M.
        let first = ledger.reserve(hash, 4_000_000, 8).unwrap();
        assert_eq!(first.attempt, 1);
        ledger.settle(first, 1_000_000);
        let entry = ledger.lookup(&hash).unwrap();
        assert_eq!((entry.work_gas_reserved, entry.work_gas_settled), (0, 1_000_000));

        assert_eq!(ledger.reserve(hash, 9_500_000, 8), Err(ReserveError::Exhausted));
        ledger.reserve(hash, 1, 8).unwrap();
        assert_eq!(ledger.reserve(hash, 1, 2), Err(ReserveError::Exhausted), "server cap applies");
        ledger.reserve(hash, 1, 8).unwrap();
        assert_eq!(ledger.reserve(hash, 1, 8), Err(ReserveError::Exhausted), "signed cap applies");
        assert_eq!(ledger.lookup(&hash).unwrap().attempts, 3);
    }

    #[test]
    fn prune_keeps_tombstones_through_the_recovery_window() {
        let ledger =
            GroupLedger::new(LedgerLimits { recovery_window_blocks: 10, ..Default::default() });
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        let sibling = group.siblings()[1].hash();
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, pooled).unwrap();
        ledger.confirm_pooled(group.group_hash());
        ledger.evict(group.group_hash(), EvictionReason::Expired);

        ledger.prune(110);
        assert!(ledger.suppresses_plain_reinsertion(&sibling));
        assert_eq!(ledger.group_of_sibling(&sibling), Some(group.group_hash()));
        ledger.prune(111);
        assert!(!ledger.suppresses_plain_reinsertion(&sibling));
        assert_eq!(ledger.counts(), (0, 0));
    }
}
