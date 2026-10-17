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
    /// A sibling was mined in a canonical block.
    Landed {
        /// The sibling that landed.
        winner: TxHash,
        /// Canonical block that included it.
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
    /// The latest sibling the builder committed for this group, and the block. Diagnostic only:
    /// a built block may never become canonical, so this is not evidence of landing.
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

    /// Returns `true` while the group is pending insertion or pooled.
    pub const fn is_live(&self) -> bool {
        matches!(self.state, GroupState::Pending | GroupState::Pooled)
    }
}

/// Result of [`GroupLedger::create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerAdmission {
    /// A new entry was created; insert the group into the pool, then confirm or abort.
    Created,
    /// The same `groupHash` is already pooled; return the original response. No slot is
    /// reserved and no budget is refreshed.
    Duplicate,
    /// The same `groupHash` is still being inserted by another request. The caller must wait
    /// for that insertion's outcome instead of reporting acceptance.
    Pending,
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
    /// Live groups the pool may hold. A same-sender, same-nonce replacement reuses its
    /// predecessor's slot.
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

/// The ledger's contents, guarded by one lock in [`GroupLedger`].
#[derive(Debug, Default)]
pub struct LedgerState {
    /// Entries by `groupHash`.
    pub entries: HashMap<B256, GroupLedgerEntry>,
    /// Sibling hash to every retained group that contains it, oldest first. A sibling stays
    /// suppressed while any owner is retained.
    pub tombstones: HashMap<TxHash, Vec<B256>>,
}

impl LedgerState {
    /// Settles pooled entries whose representative is no longer pooled as
    /// `Evicted(Dropped)`. Landing is recorded only from canonical inclusion
    /// ([`GroupLedger::record_landed`]).
    pub fn reconcile(&mut self, is_pooled: &impl Fn(&TxHash) -> bool) {
        for entry in self.entries.values_mut() {
            if entry.state == GroupState::Pooled && !is_pooled(&entry.group.representative().hash())
            {
                entry.state = GroupState::Evicted(EvictionReason::Dropped);
            }
        }
    }

    /// Returns `true` when a live entry owns `hash` as a sibling.
    pub fn has_live_owner(&self, hash: &TxHash) -> bool {
        self.tombstones.get(hash).is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| self.entries.get(owner).is_some_and(GroupLedgerEntry::is_live))
        })
    }

    /// Removes `group_hash` from every tombstone and drops tombstones left without an owner.
    pub fn release_tombstones(&mut self, group: &CandidateGroup) {
        let group_hash = group.group_hash();
        for sibling in group.siblings() {
            let hash = sibling.hash();
            if let Some(owners) = self.tombstones.get_mut(&hash) {
                owners.retain(|owner| *owner != group_hash);
                if owners.is_empty() {
                    self.tombstones.remove(&hash);
                }
            }
        }
    }
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
    /// `is_pooled` reports whether a transaction hash is in the pool. It releases capacity held
    /// by groups the pool dropped through paths that do not notify the ledger, and rejects a
    /// representative that is already pooled, whether as a plain transaction or for another
    /// group.
    pub fn create(
        &self,
        group: Arc<CandidateGroup>,
        principal: PrincipalId,
        policy_version: u64,
        is_pooled: impl Fn(&TxHash) -> bool,
    ) -> Result<LedgerAdmission, CandidateError> {
        let mut state = self.state.lock();
        state.reconcile(&is_pooled);
        let group_hash = group.group_hash();
        if let Some(existing) = state.entries.get(&group_hash) {
            return match existing.state {
                state if state.is_terminal() => Err(CandidateError::GroupTerminal),
                GroupState::Pending => Ok(LedgerAdmission::Pending),
                _ => Ok(LedgerAdmission::Duplicate),
            };
        }
        let representative = group.representative().hash();
        if is_pooled(&representative) || state.has_live_owner(&representative) {
            return Err(CandidateError::RepresentativeConflict);
        }
        let (mut sender_groups, mut total) = (0_usize, 0_usize);
        for entry in state.entries.values().filter(|entry| entry.is_live()) {
            if entry.group.sender() == group.sender() && entry.group.nonce() == group.nonce() {
                // A replacement takes over this slot when it is confirmed.
                continue;
            }
            total += 1;
            if entry.group.sender() == group.sender() {
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
            let owners = state.tombstones.entry(sibling.hash()).or_default();
            if !owners.contains(&group_hash) {
                owners.push(group_hash);
            }
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

    /// Returns the state of `group_hash` after settling entries the pool no longer holds, so a
    /// stale `Pooled` entry is never reported as accepted.
    pub fn classify(
        &self,
        group_hash: &B256,
        is_pooled: impl Fn(&TxHash) -> bool,
    ) -> Option<GroupState> {
        let mut state = self.state.lock();
        state.reconcile(&is_pooled);
        state.entries.get(group_hash).map(|entry| entry.state)
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

    /// Removes a created group whose pool insertion failed. Nothing was spent. Tombstones that
    /// other retained groups own are kept.
    pub fn abort(&self, group_hash: B256) {
        let mut state = self.state.lock();
        if state.entries.get(&group_hash).is_some_and(|entry| entry.state == GroupState::Pending)
            && let Some(entry) = state.entries.remove(&group_hash)
        {
            state.release_tombstones(&entry.group);
        }
    }

    /// Returns a snapshot of the entry for `group_hash`.
    pub fn lookup(&self, group_hash: &B256) -> Option<GroupLedgerEntry> {
        self.state.lock().entries.get(group_hash).cloned()
    }

    /// Returns the most recently admitted retained group containing a sibling hash.
    pub fn group_of_sibling(&self, hash: &TxHash) -> Option<B256> {
        self.state.lock().tombstones.get(hash).and_then(|owners| owners.last().copied())
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

    /// Returns the attempt of an interrupted round (design §5.7 step 7: a cancellation or
    /// deadline stop defers without consuming an attempt). Work already settled stays charged.
    /// Only the latest attempt can be returned.
    pub fn release_attempt(&self, group_hash: B256, attempt: u16) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(&group_hash)
            && entry.attempts == attempt
        {
            entry.attempts -= 1;
        }
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

    /// Records that the builder committed `winner` for the group in a built block. Diagnostic
    /// only; see [`Self::record_landed`] for canonical inclusion.
    pub fn record_commit(&self, group_hash: B256, winner: TxHash, block: u64) {
        if let Some(entry) = self.state.lock().entries.get_mut(&group_hash) {
            entry.last_commit = Some((winner, block));
        }
    }

    /// Marks groups whose sibling was mined in canonical `block` as `Landed`. A group the pool
    /// already settled as `Evicted(Dropped)` (its representative left with the nonce) is
    /// corrected too.
    pub fn record_landed(&self, mined: &[TxHash], block: u64) {
        let mut state = self.state.lock();
        let LedgerState { entries, tombstones } = &mut *state;
        for hash in mined {
            for owner in tombstones.get(hash).into_iter().flatten() {
                if let Some(entry) = entries.get_mut(owner)
                    && (entry.is_live()
                        || entry.state == GroupState::Evicted(EvictionReason::Dropped))
                {
                    entry.state = GroupState::Landed { winner: *hash, block };
                }
            }
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

    /// Settles pooled entries whose representative is no longer pooled as `Evicted(Dropped)`.
    pub fn reconcile(&self, is_pooled: impl Fn(&TxHash) -> bool) {
        self.state.lock().reconcile(&is_pooled);
    }

    /// Drops entries whose recovery window ended before `committed_block`, and the tombstones
    /// no retained entry owns.
    pub fn prune(&self, committed_block: u64) {
        let window = self.limits.recovery_window_blocks;
        let mut state = self.state.lock();
        state.entries.retain(|_, entry| {
            entry.group.envelope().expiry_block.saturating_add(window) >= committed_block
        });
        let LedgerState { entries, tombstones } = &mut *state;
        tombstones.retain(|_, owners| {
            owners.retain(|owner| entries.contains_key(owner));
            !owners.is_empty()
        });
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

    /// A pool holding exactly the representatives of `groups`.
    fn pool_of(groups: &[&Arc<CandidateGroup>]) -> impl Fn(&TxHash) -> bool {
        let pooled: Vec<_> = groups.iter().map(|group| group.representative().hash()).collect();
        move |hash| pooled.contains(hash)
    }

    fn empty_pool(_: &TxHash) -> bool {
        false
    }

    /// Creates and confirms `group` in a pool that holds `others` plus the group itself.
    fn admit(ledger: &GroupLedger, group: &Arc<CandidateGroup>, others: &[&Arc<CandidateGroup>]) {
        assert_eq!(
            ledger.create(Arc::clone(group), PRINCIPAL, 1, pool_of(others)),
            Ok(LedgerAdmission::Created)
        );
        ledger.confirm_pooled(group.group_hash());
    }

    #[test]
    fn exact_duplicate_does_not_reserve_or_refresh() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        admit(&ledger, &group, &[]);
        ledger.reserve(group.group_hash(), 10, 8).unwrap();
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, pool_of(&[&group])),
            Ok(LedgerAdmission::Duplicate)
        );
        let entry = ledger.lookup(&group.group_hash()).unwrap();
        assert_eq!((entry.attempts, entry.work_gas_reserved), (1, 10));
        assert_eq!(ledger.counts(), (1, 1));
    }

    #[test]
    fn pending_duplicate_is_not_an_accepted_group() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, empty_pool).unwrap();
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, empty_pool),
            Ok(LedgerAdmission::Pending)
        );
        assert_eq!(ledger.classify(&group.group_hash(), empty_pool), Some(GroupState::Pending));
    }

    #[test]
    fn stale_pooled_duplicate_is_terminal() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        admit(&ledger, &group, &[]);
        // An admin drop removed the representative without notifying the ledger.
        assert_eq!(
            ledger.classify(&group.group_hash(), empty_pool),
            Some(GroupState::Evicted(EvictionReason::Dropped))
        );
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, empty_pool),
            Err(CandidateError::GroupTerminal)
        );
    }

    #[test]
    fn terminal_group_is_rejected_on_resubmission() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        admit(&ledger, &group, &[]);
        ledger.evict(group.group_hash(), EvictionReason::BudgetExhausted);
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, empty_pool),
            Err(CandidateError::GroupTerminal)
        );
    }

    #[test]
    fn same_representative_under_different_envelope_conflicts() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let first = group(&fixture, 0, 1, 0);
        admit(&ledger, &first, &[]);

        let raw: Vec<_> = first.siblings().iter().map(|sibling| sibling.encoded.clone()).collect();
        let mut envelope = first.envelope().clone();
        envelope.max_evaluations = 5;
        let second = Arc::new(fixture.group(&raw, envelope));
        assert_eq!(
            ledger.create(second, PRINCIPAL, 1, pool_of(&[&first])),
            Err(CandidateError::RepresentativeConflict)
        );
    }

    #[test]
    fn plain_pooled_representative_conflicts() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        // The representative is pooled as an ordinary transaction; the ledger has no entry.
        assert_eq!(
            ledger.create(Arc::clone(&group), PRINCIPAL, 1, pool_of(&[&group])),
            Err(CandidateError::RepresentativeConflict)
        );
        assert_eq!(ledger.counts(), (0, 0));
    }

    #[test]
    fn sender_limit_counts_other_nonces_and_allows_replacement() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let first = group(&fixture, 0, 1, 0);
        admit(&ledger, &first, &[]);

        let other_nonce = group(&fixture, 1, 10, 0);
        assert_eq!(
            ledger.create(other_nonce, PRINCIPAL, 1, pool_of(&[&first])),
            Err(CandidateError::SenderGroupLimit)
        );

        let replacement = group(&fixture, 0, 20, 0);
        admit(&ledger, &replacement, &[&first]);
        assert_eq!(ledger.lookup(&first.group_hash()).unwrap().state, GroupState::Replaced);
        assert_eq!(ledger.live_group(fixture.address(), 0), Some(replacement.group_hash()));
    }

    #[test]
    fn replacement_reuses_its_slot_at_the_global_cap() {
        let ledger = GroupLedger::new(LedgerLimits { max_groups: 1, ..Default::default() });
        let fixture = CandidateFixture::new(CHAIN_ID);
        let first = group(&fixture, 0, 1, 0);
        admit(&ledger, &first, &[]);
        let replacement = group(&fixture, 0, 20, 0);
        admit(&ledger, &replacement, &[&first]);
        assert_eq!(ledger.counts(), (1, 2));
    }

    #[test]
    fn dropped_groups_release_capacity_lazily() {
        let ledger = GroupLedger::new(LedgerLimits { max_groups: 1, ..Default::default() });
        let fixture = CandidateFixture::new(CHAIN_ID);
        let other = CandidateFixture::with_key(CHAIN_ID, 0x55);
        let first = group(&fixture, 0, 1, 0);
        admit(&ledger, &first, &[]);
        assert_eq!(
            ledger.create(group(&other, 0, 1, 0), PRINCIPAL, 1, pool_of(&[&first])),
            Err(CandidateError::PoolGroupLimit)
        );
        ledger.create(group(&other, 0, 1, 0), PRINCIPAL, 1, empty_pool).unwrap();
        assert_eq!(
            ledger.lookup(&first.group_hash()).unwrap().state,
            GroupState::Evicted(EvictionReason::Dropped)
        );
    }

    #[test]
    fn provisional_commit_is_not_landing() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        admit(&ledger, &group, &[]);
        let winner = group.siblings()[1].hash();
        // The built block was abandoned; the representative later left the pool another way.
        ledger.record_commit(group.group_hash(), winner, 12);
        ledger.reconcile(empty_pool);
        assert_eq!(
            ledger.lookup(&group.group_hash()).unwrap().state,
            GroupState::Evicted(EvictionReason::Dropped)
        );
    }

    #[test]
    fn canonical_inclusion_marks_group_landed() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        admit(&ledger, &group, &[]);
        let winner = group.siblings()[1].hash();
        // Pool maintenance pruned the representative before the ledger saw the mined block.
        ledger.reconcile(empty_pool);
        ledger.record_landed(&[winner], 12);
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
        ledger.create(Arc::clone(&group), PRINCIPAL, 1, empty_pool).unwrap();
        ledger.abort(group.group_hash());
        assert!(ledger.lookup(&group.group_hash()).is_none());
        assert!(!ledger.suppresses_plain_reinsertion(&group.siblings()[1].hash()));
    }

    /// A terminal group B and a failed admission C both contain sibling `s`.
    fn overlapping_groups(
        fixture: &CandidateFixture,
    ) -> (Arc<CandidateGroup>, Arc<CandidateGroup>, TxHash) {
        let shared = fixture.encode(fixture.sibling_tx(0, Address::repeat_byte(0x30), U256::ZERO));
        let b_raw = vec![
            fixture.encode(fixture.sibling_tx(0, Address::repeat_byte(0x10), U256::ZERO)),
            shared.clone(),
        ];
        let c_raw = vec![
            fixture.encode(fixture.sibling_tx(0, Address::repeat_byte(0x20), U256::ZERO)),
            shared,
        ];
        let b = Arc::new(fixture.group(&b_raw, fixture.envelope_for(&b_raw, 0)));
        let c = Arc::new(fixture.group(&c_raw, fixture.envelope_for(&c_raw, 0)));
        let sibling = b.siblings()[1].hash();
        assert_eq!(sibling, c.siblings()[1].hash());
        (b, c, sibling)
    }

    #[test]
    fn failed_overlapping_admission_keeps_original_tombstone() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (b, c, sibling) = overlapping_groups(&fixture);
        admit(&ledger, &b, &[]);
        ledger.evict(b.group_hash(), EvictionReason::BudgetExhausted);

        ledger.create(Arc::clone(&c), PRINCIPAL, 1, empty_pool).unwrap();
        ledger.abort(c.group_hash());

        assert!(ledger.suppresses_plain_reinsertion(&sibling));
        assert_eq!(ledger.group_of_sibling(&sibling), Some(b.group_hash()));
    }

    #[test]
    fn overlapping_tombstone_survives_until_last_owner_is_pruned() {
        let ledger =
            GroupLedger::new(LedgerLimits { recovery_window_blocks: 10, ..Default::default() });
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (b, _, sibling) = overlapping_groups(&fixture);
        let c_raw: Vec<_> = vec![
            fixture.encode(fixture.sibling_tx(0, Address::repeat_byte(0x20), U256::ZERO)),
            b.siblings()[1].encoded.clone(),
        ];
        let mut envelope = fixture.envelope_for(&c_raw, 0);
        envelope.expiry_block = 200;
        let c = Arc::new(fixture.group(&c_raw, envelope));
        admit(&ledger, &b, &[]);
        ledger.evict(b.group_hash(), EvictionReason::Expired);
        admit(&ledger, &c, &[]);
        assert_eq!(ledger.group_of_sibling(&sibling), Some(c.group_hash()));

        ledger.prune(111);
        assert!(ledger.lookup(&b.group_hash()).is_none());
        assert_eq!(ledger.group_of_sibling(&sibling), Some(c.group_hash()));
        ledger.prune(211);
        assert!(!ledger.suppresses_plain_reinsertion(&sibling));
    }

    #[test]
    fn interrupted_round_returns_its_attempt_but_keeps_its_work() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        let hash = group.group_hash();
        admit(&ledger, &group, &[]);

        let reservation = ledger.reserve(hash, 100, 8).unwrap();
        let attempt = reservation.attempt;
        ledger.settle(reservation, 40);
        ledger.release_attempt(hash, attempt);

        let entry = ledger.lookup(&hash).unwrap();
        assert_eq!((entry.attempts, entry.work_gas_settled, entry.work_gas_reserved), (0, 40, 0));
    }

    #[test]
    fn reservations_enforce_attempts_and_work_and_settle_refunds() {
        let ledger = GroupLedger::default();
        let fixture = CandidateFixture::new(CHAIN_ID);
        let group = group(&fixture, 0, 1, 0);
        let hash = group.group_hash();
        admit(&ledger, &group, &[]);

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
        admit(&ledger, &group, &[]);
        ledger.evict(group.group_hash(), EvictionReason::Expired);

        ledger.prune(110);
        assert!(ledger.suppresses_plain_reinsertion(&sibling));
        assert_eq!(ledger.group_of_sibling(&sibling), Some(group.group_hash()));
        ledger.prune(111);
        assert!(!ledger.suppresses_plain_reinsertion(&sibling));
        assert_eq!(ledger.counts(), (0, 0));
    }
}
