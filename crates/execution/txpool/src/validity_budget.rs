//! Occupancy budget for validity-predicate transactions in the shared protocol pool.
//!
//! Validity transactions live in reth's protocol pool next to standard user
//! transactions, and reth's subpool limits evict by priority without regard to
//! kind. A validity transaction whose predicates never hold stays pending until
//! its block deadline and costs its sender nothing, so a flood of them at a high
//! priority fee can push standard transactions out of the pool.
//!
//! [`ValidityBudget`] caps how many validity transactions, and how many bytes of
//! them, the protocol pool may hold. Once the budget is full a newcomer must
//! strictly outbid the lowest-priority resident, which is then evicted; an
//! equal-priced newcomer is rejected so a sender cannot rotate the resident set
//! for free. Standard transactions are never counted, so they always keep
//! `pending_limit - budget` slots to themselves.
//!
//! The budget indexes hashes only. The pool owns the transactions and drives
//! every mutation: [`ValidityBudget::plan_admission`] under the admission lock,
//! [`ValidityBudget::commit`] once reth has accepted the transaction, and
//! [`ValidityBudget::remove`] or [`ValidityBudget::retain`] when transactions
//! leave the pool by inclusion, replacement, expiry, or reth's own eviction.

use std::collections::{BTreeMap, HashMap};

use alloy_primitives::TxHash;
use reth_eth_wire_types::HandleMempoolData;
use reth_transaction_pool::SubPoolLimit;

/// Why a validity transaction cannot be admitted under the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValidityBudgetRejection {
    /// The transaction alone exceeds the budget's byte limit.
    #[error(
        "validity transaction of {size} bytes exceeds the validity pool size limit of {max_size} bytes"
    )]
    Oversized {
        /// Encoded size of the rejected transaction.
        size: usize,
        /// Byte limit of the budget.
        max_size: usize,
    },
    /// The budget is full and the transaction does not outbid enough residents.
    #[error(
        "validity pool is full and the transaction does not outbid the lowest-priority validity transaction"
    )]
    Underpriced,
}

impl ValidityBudgetRejection {
    /// Static metric label for this rejection.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Oversized { .. } => "oversized",
            Self::Underpriced => "underpriced",
        }
    }
}

/// A planned admission, computed by [`ValidityBudget::plan_admission`] and applied by
/// [`ValidityBudget::commit`] once the pool has accepted the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidityAdmission {
    hash: TxHash,
    priority: u128,
    size: usize,
    evicted: Vec<TxHash>,
}

impl ValidityAdmission {
    /// Hash of the transaction being admitted.
    pub const fn hash(&self) -> TxHash {
        self.hash
    }

    /// Residents that must leave the pool to make room, lowest priority first.
    pub fn evicted(&self) -> &[TxHash] {
        &self.evicted
    }
}

/// Budget hashes under reconciliation. Implements [`HandleMempoolData`] so reth's
/// `retain_unknown` can prune the still-pooled ones under a single pool lock,
/// leaving the hashes the budget counts but the pool no longer holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackedValidityHashes(pub Vec<TxHash>);

impl HandleMempoolData for TrackedValidityHashes {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn retain_by_hash(&mut self, mut f: impl FnMut(&TxHash) -> bool) {
        self.0.retain(|hash| f(hash));
    }
}

/// Operator bounds for the validity budget. Each unset bound defaults to a share of
/// the pending subpool limit, so the budget is on by default and tracks the pool it
/// protects: a node with the reth defaults gets 5,000 transactions / 10 MB, mainnet's
/// 100,000 / 50 MB pending pool gets 50,000 / 25 MB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidityLimitOverrides {
    /// Maximum validity transactions, or `None` for the default share of the pending limit.
    pub max_txs: Option<usize>,
    /// Maximum combined encoded bytes, or `None` for the default share of the pending limit.
    pub max_size: Option<usize>,
}

impl ValidityLimitOverrides {
    /// Unset bounds default to the pending limit divided by this, so standard
    /// transactions always keep at least half of the pending pool.
    pub const PENDING_DIVISOR: usize = 2;

    /// Resolves the budget against the configured pending subpool limit.
    #[must_use]
    pub fn resolve(self, pending: SubPoolLimit) -> SubPoolLimit {
        SubPoolLimit::new(
            self.max_txs.unwrap_or(pending.max_txs / Self::PENDING_DIVISOR),
            self.max_size.unwrap_or(pending.max_size / Self::PENDING_DIVISOR),
        )
    }
}

/// Count and byte budget for validity transactions in the protocol pool.
///
/// Residents are ordered by `(priority, hash)` ascending so the cheapest is
/// evicted first; `priority` is the transaction's maximum priority fee per gas,
/// the same bid reth's pending pool evicts on.
#[derive(Debug)]
pub struct ValidityBudget {
    limit: SubPoolLimit,
    /// Ordered by `(priority, hash)` ascending; the front is evicted first. The
    /// value is the transaction's encoded size.
    by_priority: BTreeMap<(u128, TxHash), usize>,
    /// `hash -> (priority, size)` for O(1) removal and idempotency.
    entries: HashMap<TxHash, (u128, usize)>,
    size: usize,
}

impl ValidityBudget {
    /// Creates an empty budget with the given limits.
    #[must_use]
    pub fn new(limit: SubPoolLimit) -> Self {
        Self { limit, by_priority: BTreeMap::new(), entries: HashMap::new(), size: 0 }
    }

    /// Creates a budget that never rejects or evicts.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(SubPoolLimit::new(usize::MAX, usize::MAX))
    }

    /// Configured limits.
    #[must_use]
    pub const fn limit(&self) -> SubPoolLimit {
        self.limit
    }

    /// Number of validity transactions currently counted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no validity transaction is counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total encoded bytes of the counted validity transactions.
    #[must_use]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Returns `true` if `hash` is counted.
    #[must_use]
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Counted hashes in eviction order, lowest priority first.
    #[must_use]
    pub fn tracked_hashes(&self) -> Vec<TxHash> {
        self.by_priority.keys().map(|(_, hash)| *hash).collect()
    }

    /// Plans admitting `hash` with the given bid and size, crediting `replaced` if
    /// it is a counted resident this admission supersedes.
    ///
    /// Residents strictly below `priority` are evicted from the cheapest up until
    /// the newcomer fits. Nothing is mutated; apply the plan with
    /// [`Self::commit`] after the pool has accepted the transaction.
    pub fn plan_admission(
        &self,
        hash: TxHash,
        priority: u128,
        size: usize,
        replaced: Option<&TxHash>,
    ) -> Result<ValidityAdmission, ValidityBudgetRejection> {
        if size > self.limit.max_size {
            return Err(ValidityBudgetRejection::Oversized { size, max_size: self.limit.max_size });
        }
        let (mut count, mut bytes) = (self.entries.len(), self.size);
        if let Some((_, replaced_size)) = replaced.and_then(|replaced| self.entries.get(replaced)) {
            count -= 1;
            bytes -= replaced_size;
        }
        let mut evicted = Vec::new();
        for ((resident_priority, resident), resident_size) in &self.by_priority {
            if count < self.limit.max_txs && bytes + size <= self.limit.max_size {
                break;
            }
            if *resident_priority >= priority {
                return Err(ValidityBudgetRejection::Underpriced);
            }
            if replaced == Some(resident) {
                continue;
            }
            evicted.push(*resident);
            count -= 1;
            bytes -= resident_size;
        }
        if count >= self.limit.max_txs || bytes + size > self.limit.max_size {
            return Err(ValidityBudgetRejection::Underpriced);
        }
        Ok(ValidityAdmission { hash, priority, size, evicted })
    }

    /// Applies a plan: drops its evictions and counts the newcomer. Returns the
    /// evicted hashes that were still counted, which the caller must remove
    /// from the pool.
    pub fn commit(&mut self, admission: ValidityAdmission) -> Vec<TxHash> {
        let mut evicted = admission.evicted;
        evicted.retain(|hash| self.remove(hash));
        self.insert(admission.hash, admission.priority, admission.size);
        evicted
    }

    /// Counts `hash` without checking the limit. Idempotent for an existing hash.
    pub fn insert(&mut self, hash: TxHash, priority: u128, size: usize) {
        if self.entries.contains_key(&hash) {
            return;
        }
        self.by_priority.insert((priority, hash), size);
        self.entries.insert(hash, (priority, size));
        self.size += size;
    }

    /// Stops counting `hash`. Returns `true` if it was counted.
    pub fn remove(&mut self, hash: &TxHash) -> bool {
        let Some((priority, size)) = self.entries.remove(hash) else {
            return false;
        };
        self.by_priority.remove(&(priority, *hash));
        self.size -= size;
        true
    }

    /// Drops every counted hash for which `keep` returns `false`, returning them.
    /// Reconciles the budget with evictions the pool performed on its own.
    pub fn retain(&mut self, mut keep: impl FnMut(&TxHash) -> bool) -> Vec<TxHash> {
        let stale: Vec<TxHash> = self.entries.keys().filter(|hash| !keep(hash)).copied().collect();
        for hash in &stale {
            self.remove(hash);
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    fn filled(limit: SubPoolLimit, residents: &[(u8, u128, usize)]) -> ValidityBudget {
        let mut budget = ValidityBudget::new(limit);
        for (byte, priority, size) in residents {
            budget.insert(hash(*byte), *priority, *size);
        }
        budget
    }

    #[test]
    fn admits_while_under_both_limits() {
        let budget = filled(SubPoolLimit::new(2, 100), &[(1, 5, 40)]);
        let admission = budget.plan_admission(hash(2), 1, 60, None).unwrap();
        assert!(admission.evicted().is_empty());
        assert_eq!(admission.hash(), hash(2));
    }

    #[test]
    fn count_cap_evicts_cheapest_resident_for_a_higher_bid() {
        let mut budget = filled(SubPoolLimit::new(2, usize::MAX), &[(1, 5, 10), (2, 7, 10)]);
        let admission = budget.plan_admission(hash(3), 6, 10, None).unwrap();
        assert_eq!(admission.evicted(), &[hash(1)]);
        assert_eq!(budget.commit(admission), vec![hash(1)]);
        // Lowest bid first: the newcomer at 6 precedes the resident at 7.
        assert_eq!(budget.tracked_hashes(), vec![hash(3), hash(2)]);
        assert_eq!(budget.len(), 2);
        assert_eq!(budget.size(), 20);
    }

    #[test]
    fn count_cap_rejects_a_bid_that_does_not_strictly_outbid_the_cheapest() {
        let budget = filled(SubPoolLimit::new(2, usize::MAX), &[(1, 5, 10), (2, 7, 10)]);
        assert_eq!(
            budget.plan_admission(hash(3), 5, 10, None),
            Err(ValidityBudgetRejection::Underpriced),
            "equal bids must not rotate the resident set"
        );
        assert_eq!(
            budget.plan_admission(hash(3), 4, 10, None),
            Err(ValidityBudgetRejection::Underpriced)
        );
    }

    #[test]
    fn size_cap_evicts_as_many_cheap_residents_as_the_newcomer_needs() {
        let mut budget =
            filled(SubPoolLimit::new(usize::MAX, 100), &[(1, 1, 30), (2, 2, 30), (3, 9, 30)]);
        let admission = budget.plan_admission(hash(4), 5, 60, None).unwrap();
        assert_eq!(admission.evicted(), &[hash(1), hash(2)]);
        budget.commit(admission);
        assert_eq!(budget.size(), 90);
        assert_eq!(budget.tracked_hashes(), vec![hash(4), hash(3)]);
    }

    #[test]
    fn size_cap_rejects_when_cheaper_residents_cannot_free_enough_bytes() {
        let budget = filled(SubPoolLimit::new(usize::MAX, 100), &[(1, 1, 10), (2, 9, 80)]);
        assert_eq!(
            budget.plan_admission(hash(3), 5, 50, None),
            Err(ValidityBudgetRejection::Underpriced)
        );
    }

    #[test]
    fn oversized_transaction_is_rejected_even_into_an_empty_budget() {
        let budget = ValidityBudget::new(SubPoolLimit::new(10, 100));
        assert_eq!(
            budget.plan_admission(hash(1), u128::MAX, 101, None),
            Err(ValidityBudgetRejection::Oversized { size: 101, max_size: 100 })
        );
    }

    #[test]
    fn replacement_is_credited_before_evicting_others() {
        let mut budget = filled(SubPoolLimit::new(2, usize::MAX), &[(1, 5, 10), (2, 7, 10)]);
        // Replacing the cheapest resident at a lower bid still fits because its
        // own slot is freed; nothing else is evicted.
        let admission = budget.plan_admission(hash(3), 1, 10, Some(&hash(1))).unwrap();
        assert!(admission.evicted().is_empty());
        budget.remove(&hash(1));
        budget.commit(admission);
        assert_eq!(budget.tracked_hashes(), vec![hash(3), hash(2)]);
    }

    #[test]
    fn replacement_of_an_uncounted_transaction_gets_no_credit() {
        let budget = filled(SubPoolLimit::new(1, usize::MAX), &[(1, 5, 10)]);
        assert_eq!(
            budget.plan_admission(hash(3), 5, 10, Some(&hash(9))),
            Err(ValidityBudgetRejection::Underpriced)
        );
    }

    #[test]
    fn commit_skips_evictions_already_released() {
        let mut budget = filled(SubPoolLimit::new(1, usize::MAX), &[(1, 5, 10)]);
        let admission = budget.plan_admission(hash(2), 6, 10, None).unwrap();
        assert!(budget.remove(&hash(1)), "released by the pool between plan and commit");
        assert!(budget.commit(admission).is_empty());
        assert_eq!(budget.tracked_hashes(), vec![hash(2)]);
    }

    #[test]
    fn insert_is_idempotent_and_remove_is_exact() {
        let mut budget = ValidityBudget::new(SubPoolLimit::new(10, 1_000));
        budget.insert(hash(1), 5, 10);
        budget.insert(hash(1), 99, 999);
        assert_eq!((budget.len(), budget.size()), (1, 10));
        assert!(budget.remove(&hash(1)));
        assert!(!budget.remove(&hash(1)));
        assert!(budget.is_empty());
        assert_eq!(budget.size(), 0);
    }

    #[test]
    fn retain_drops_only_rejected_hashes() {
        let mut budget =
            filled(SubPoolLimit::new(10, 1_000), &[(1, 1, 10), (2, 2, 20), (3, 3, 30)]);
        let mut stale = budget.retain(|hash| *hash != TxHash::repeat_byte(2));
        stale.sort();
        assert_eq!(stale, vec![hash(2)]);
        assert_eq!(budget.tracked_hashes(), vec![hash(1), hash(3)]);
        assert_eq!(budget.size(), 40);
    }

    #[test]
    fn unlimited_budget_never_evicts_or_rejects() {
        let mut budget = ValidityBudget::unlimited();
        for byte in 0..=u8::MAX {
            let admission = budget.plan_admission(hash(byte), 0, usize::MAX / 1024, None).unwrap();
            assert!(admission.evicted().is_empty());
            budget.commit(admission);
        }
        assert_eq!(budget.len(), 256);
    }

    #[test]
    fn unset_overrides_take_half_of_the_pending_limit() {
        let pending = SubPoolLimit::new(100_000, 50 * 1024 * 1024);
        assert_eq!(
            ValidityLimitOverrides::default().resolve(pending),
            SubPoolLimit::new(50_000, 25 * 1024 * 1024)
        );
        assert_eq!(
            ValidityLimitOverrides::default().resolve(SubPoolLimit::default()),
            SubPoolLimit::new(5_000, 10 * 1024 * 1024)
        );
    }

    #[test]
    fn set_overrides_win_independently() {
        let pending = SubPoolLimit::new(100_000, 50 * 1024 * 1024);
        let overrides = ValidityLimitOverrides { max_txs: Some(7), max_size: None };
        assert_eq!(overrides.resolve(pending), SubPoolLimit::new(7, 25 * 1024 * 1024));
        let overrides = ValidityLimitOverrides { max_txs: None, max_size: Some(9) };
        assert_eq!(overrides.resolve(pending), SubPoolLimit::new(50_000, 9));
    }
}
