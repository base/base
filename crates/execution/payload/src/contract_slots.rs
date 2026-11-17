//! Per-contract changed-slot limits and their per-block accounting.
//!
//! A storage trie's update cost grows with the number of changed slots and the
//! depth of the trie. These limits count changed storage slots per storage owner
//! (the account whose storage changed, not the sender or callee) in the
//! executed post-state. A flat cap applies to every owner; an optional
//! trie-depth table assigns listed owners a depth class, and a node budget is
//! compiled at schedule load into one changed-slot cap per depth class.
//!
//! The limits are dry run only: an overrun is recorded and never excludes,
//! skips, or evicts a transaction.

use alloy_primitives::{Address, map::AddressMap};
use revm::state::EvmState;
use serde::{Deserialize, Serialize};

use crate::{ResourceMeteringError, ResourceThrottlingLimitScope};

/// Largest storage-trie depth class a schedule may name.
///
/// A storage key is 64 nibbles, so no path has more branch nodes.
pub const MAX_TRIE_DEPTH: u8 = 64;

/// Largest number of contracts a trie-depth table may list.
pub const MAX_TRIE_DEPTH_CONTRACTS: usize = 100_000;

/// Per-contract changed-slot limits from a resource-metering schedule.
///
/// JSON key `contractSlots`. Hand-built limits must go through
/// [`Self::compile`] before [`Self::check`]; schedule loaders do this.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContractSlotLimits {
    /// Observe overruns without excluding. Must be `true`: enforcement is not supported.
    pub dry_run: bool,
    /// Cap on changed slots per storage owner, per transaction and per block.
    ///
    /// Applies to owners without a tighter trie-depth cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_slot_limit: Option<u64>,
    /// Depth classes for listed owners and the node budget that sets their caps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trie_depth: Option<TrieDepthTable>,
    /// Compiled cap and depth class for each listed owner whose depth class has a
    /// cap. Owners whose class has no cap are omitted and fall back to
    /// [`Self::changed_slot_limit`].
    #[serde(skip)]
    owner_caps: AddressMap<ContractSlotCap>,
}

/// File form of [`ContractSlotLimits`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContractSlotLimitsFile {
    dry_run: bool,
    #[serde(default)]
    changed_slot_limit: Option<u64>,
    #[serde(default)]
    trie_depth: Option<TrieDepthTable>,
}

/// Storage-trie depth classes for listed contracts, and the node budget that
/// compiles into one changed-slot cap per class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrieDepthTable {
    /// Largest expected number of distinct branch nodes one owner's changed
    /// slots may touch, per transaction and per block.
    pub node_budget: u64,
    /// Contracts grouped by depth class.
    pub classes: Vec<TrieDepthClass>,
}

/// Contracts whose storage tries share one depth class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrieDepthClass {
    /// Probed branch-node depth of a random path in each contract's storage trie.
    pub depth: u8,
    /// Storage owners in this class.
    pub contracts: Vec<Address>,
}

/// Compiled changed-slot cap for one storage owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractSlotCap {
    /// Depth class from the trie-depth table, or `None` for the flat cap.
    pub depth: Option<u8>,
    /// Largest number of changed slots allowed in the owner's storage.
    pub limit: u64,
}

/// A storage owner whose changed slots exceed its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractSlotLimitExceeded {
    /// Storage owner whose slots changed.
    pub owner: Address,
    /// Depth class of the owner, or `None` when the flat cap applied.
    pub depth: Option<u8>,
    /// Whether the transaction alone or the block total exceeded the cap.
    pub scope: ResourceThrottlingLimitScope,
    /// Changed slots after adding the transaction, or the transaction's own
    /// count for a transaction-scope overrun.
    pub used: u64,
    /// Changed slots the transaction wrote in this owner's storage.
    pub transaction_slots: u64,
    /// Cap that was exceeded.
    pub limit: u64,
}

/// Per-block changed-slot counts per storage owner.
///
/// One value lives on the payload's execution info for one block. The executed
/// check stages an attempt's counts in [`Self::staged`]; [`Self::commit`] adds
/// them to [`Self::committed`] only after the transaction is committed, so
/// attempts that are executed and not included leave the totals unchanged.
/// The type is deliberately not `Clone`: it is updated in place.
#[derive(Debug, Default)]
pub struct ContractSlotUsage {
    /// Changed slots committed in this block, keyed by storage owner. Counts
    /// are additive across transactions: a slot changed by two transactions
    /// counts twice.
    pub committed: AddressMap<u64>,
    /// Changed slots per storage owner for the most recent executed attempt.
    pub staged: Vec<(Address, u64)>,
    /// Executed attempts in this block that a per-contract cap would have limited.
    pub would_limit: u64,
}

impl ContractSlotLimits {
    /// Builds dry-run limits from a flat cap and an optional trie-depth table.
    ///
    /// Call [`Self::compile`] before [`Self::check`].
    pub fn new(changed_slot_limit: Option<u64>, trie_depth: Option<TrieDepthTable>) -> Self {
        Self { dry_run: true, changed_slot_limit, trie_depth, owner_caps: AddressMap::default() }
    }

    pub(crate) fn from_file(file: ContractSlotLimitsFile) -> Self {
        Self {
            dry_run: file.dry_run,
            changed_slot_limit: file.changed_slot_limit,
            trie_depth: file.trie_depth,
            owner_caps: AddressMap::default(),
        }
    }

    /// Validates the limits and compiles the trie-depth table into per-owner caps.
    ///
    /// Each listed owner's cap is the smaller of its depth class's cap and the
    /// flat [`Self::changed_slot_limit`].
    pub fn compile(mut self) -> Result<Self, ResourceMeteringError> {
        if !self.dry_run {
            return Err(ResourceMeteringError::ContractSlotsNotDryRun);
        }
        if self.changed_slot_limit.is_none() && self.trie_depth.is_none() {
            return Err(ResourceMeteringError::EmptyContractSlotLimits);
        }
        if self.changed_slot_limit == Some(0) {
            return Err(ResourceMeteringError::ZeroContractSlotLimit);
        }

        let mut owner_caps = AddressMap::default();
        if let Some(table) = &self.trie_depth {
            if table.node_budget == 0 {
                return Err(ResourceMeteringError::ZeroContractSlotLimit);
            }
            let listed: usize = table.classes.iter().map(|class| class.contracts.len()).sum();
            if listed > MAX_TRIE_DEPTH_CONTRACTS {
                return Err(ResourceMeteringError::TooManyTrieDepthContracts(listed));
            }
            owner_caps.reserve(listed);
            let mut seen_depths = [false; MAX_TRIE_DEPTH as usize + 1];
            let mut seen_contracts = AddressMap::<()>::default();
            seen_contracts.reserve(listed);
            for class in &table.classes {
                if class.depth == 0 || class.depth > MAX_TRIE_DEPTH {
                    return Err(ResourceMeteringError::InvalidTrieDepth(class.depth));
                }
                if std::mem::replace(&mut seen_depths[usize::from(class.depth)], true) {
                    return Err(ResourceMeteringError::DuplicateTrieDepthClass(class.depth));
                }
                let depth_cap = TrieDepthTable::slot_cap(table.node_budget, class.depth);
                let limit = match (depth_cap, self.changed_slot_limit) {
                    (Some(depth_cap), Some(flat)) => Some(depth_cap.min(flat)),
                    (depth_cap, None) => depth_cap,
                    (None, Some(_)) => None,
                };
                for contract in &class.contracts {
                    if seen_contracts.insert(*contract, ()).is_some() {
                        return Err(ResourceMeteringError::DuplicateTrieDepthContract(*contract));
                    }
                    if let Some(limit) = limit {
                        owner_caps
                            .insert(*contract, ContractSlotCap { depth: Some(class.depth), limit });
                    }
                }
            }
        }
        self.owner_caps = owner_caps;
        Ok(self)
    }

    /// Returns the cap that applies to `owner`, if any.
    pub fn cap_for(&self, owner: &Address) -> Option<ContractSlotCap> {
        self.owner_caps
            .get(owner)
            .copied()
            .or_else(|| self.changed_slot_limit.map(|limit| ContractSlotCap { depth: None, limit }))
    }

    /// Checks the staged attempt against per-owner caps and block totals.
    ///
    /// Returns the first transaction-scope overrun, or else the first
    /// block-scope overrun, in staged order.
    pub fn check(&self, usage: &ContractSlotUsage) -> Option<ContractSlotLimitExceeded> {
        let mut block_overrun = None;
        for &(owner, transaction_slots) in &usage.staged {
            let Some(cap) = self.cap_for(&owner) else {
                continue;
            };
            if transaction_slots > cap.limit {
                return Some(ContractSlotLimitExceeded {
                    owner,
                    depth: cap.depth,
                    scope: ResourceThrottlingLimitScope::Transaction,
                    used: transaction_slots,
                    transaction_slots,
                    limit: cap.limit,
                });
            }
            if block_overrun.is_some() {
                continue;
            }
            let committed = usage.committed.get(&owner).copied().unwrap_or_default();
            // Saturating: an overflowing total is past any cap.
            let used = committed.saturating_add(transaction_slots);
            if used > cap.limit {
                block_overrun = Some(ContractSlotLimitExceeded {
                    owner,
                    depth: cap.depth,
                    scope: ResourceThrottlingLimitScope::Block,
                    used,
                    transaction_slots,
                    limit: cap.limit,
                });
            }
        }
        block_overrun
    }
}

impl TrieDepthTable {
    /// Expected number of distinct branch nodes on `slots` random paths in a
    /// 16-ary trie that is full to `depth` levels.
    ///
    /// `U(k, d) = Σ_{l=0}^{d-1} 16^l · (1 − (1 − 16^−l)^k)`.
    pub fn expected_branch_nodes(slots: u64, depth: u8) -> f64 {
        let slots = slots as f64;
        (0..i32::from(depth))
            .map(|level| {
                let width = 16f64.powi(level);
                // (1 - 1/width)^slots, computed in log space for deep levels.
                let untouched = (slots * (-width.recip()).ln_1p()).exp();
                width * (1.0 - untouched)
            })
            .sum()
    }

    /// Largest changed-slot count whose expected branch nodes fit `node_budget`
    /// at `depth`, or `None` when the whole trie fits the budget.
    ///
    /// Runs once per depth class at schedule load, not per transaction.
    pub fn slot_cap(node_budget: u64, depth: u8) -> Option<u64> {
        let budget = node_budget as f64;
        let total_nodes: f64 = (0..i32::from(depth)).map(|level| 16f64.powi(level)).sum();
        if total_nodes <= budget {
            return None;
        }
        // Expected nodes rise with slots toward `total_nodes`, so search the largest fit.
        let (mut low, mut high) = (0u64, u64::from(u32::MAX));
        if Self::expected_branch_nodes(high, depth) <= budget {
            return None;
        }
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if Self::expected_branch_nodes(mid, depth) <= budget {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        Some(low)
    }
}

impl ContractSlotUsage {
    /// Stages one executed attempt's changed-slot counts per storage owner.
    ///
    /// Replaces any previously staged attempt. Reuses the staging allocation.
    pub fn stage(&mut self, state: &EvmState) {
        self.staged.clear();
        for (owner, account) in state {
            let changed = account.storage.values().filter(|slot| slot.is_changed()).count() as u64;
            if changed > 0 {
                self.staged.push((*owner, changed));
            }
        }
    }

    /// Adds the staged counts to the block totals and clears the stage.
    ///
    /// An overflowing owner saturates at `u64::MAX` and the method returns
    /// [`ResourceMeteringError::ArithmeticOverflow`] after applying the rest.
    pub fn commit(&mut self) -> Result<(), ResourceMeteringError> {
        let mut overflowed = false;
        for (owner, slots) in self.staged.drain(..) {
            let total = self.committed.entry(owner).or_default();
            *total = total.checked_add(slots).unwrap_or_else(|| {
                overflowed = true;
                u64::MAX
            });
        }
        if overflowed { Err(ResourceMeteringError::ArithmeticOverflow) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use revm::state::{Account, EvmStorageSlot, TransactionId};

    use super::*;

    fn state_with_changed_slots(writes: &[(Address, u64)]) -> EvmState {
        let mut state = EvmState::default();
        for &(owner, slots) in writes {
            let mut account = Account::default();
            account.mark_touch();
            for slot in 0..slots {
                account.storage.insert(
                    U256::from(slot),
                    EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), TransactionId::ZERO),
                );
            }
            // A loaded but unchanged slot must not count.
            account.storage.insert(
                U256::from(u64::MAX),
                EvmStorageSlot::new_changed(U256::from(5), U256::from(5), TransactionId::ZERO),
            );
            state.insert(owner, account);
        }
        state
    }

    fn depth_table(node_budget: u64, classes: &[(u8, &[Address])]) -> TrieDepthTable {
        TrieDepthTable {
            node_budget,
            classes: classes
                .iter()
                .map(|(depth, contracts)| TrieDepthClass {
                    depth: *depth,
                    contracts: contracts.to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn node_budget_compiles_to_per_depth_slot_caps() {
        // Published G1 caps for a 2,100-node budget; depth 3 and shallower fit whole.
        let expected =
            [(1, None), (2, None), (3, None), (4, Some(2_419)), (5, Some(973)), (6, Some(632))];
        for (depth, cap) in expected {
            assert_eq!(TrieDepthTable::slot_cap(2_100, depth), cap, "depth {depth}");
        }
        assert_eq!(TrieDepthTable::slot_cap(2_100, 7), Some(473));
        assert_eq!(TrieDepthTable::slot_cap(2_100, 8), Some(380));
        assert_eq!(TrieDepthTable::slot_cap(2_500, 8), Some(459));
        assert_eq!(TrieDepthTable::slot_cap(1_200, 4), Some(1_056));
    }

    #[test]
    fn compiled_table_assigns_caps_by_depth_class_and_falls_back_to_flat_limit() {
        let deep = Address::repeat_byte(0xd8);
        let mid = Address::repeat_byte(0xd5);
        let shallow = Address::repeat_byte(0xd2);
        let unlisted = Address::repeat_byte(0x01);
        let limits = ContractSlotLimits::new(
            Some(900),
            Some(depth_table(2_100, &[(8, &[deep]), (5, &[mid]), (2, &[shallow])])),
        )
        .compile()
        .unwrap();

        assert_eq!(limits.cap_for(&deep), Some(ContractSlotCap { depth: Some(8), limit: 380 }));
        // The flat limit is tighter than depth 5's 973 cap; the owner keeps its class.
        assert_eq!(limits.cap_for(&mid), Some(ContractSlotCap { depth: Some(5), limit: 900 }));
        assert_eq!(limits.cap_for(&shallow), Some(ContractSlotCap { depth: None, limit: 900 }));
        assert_eq!(limits.cap_for(&unlisted), Some(ContractSlotCap { depth: None, limit: 900 }));

        let table_only = ContractSlotLimits::new(
            None,
            Some(depth_table(2_100, &[(8, &[deep]), (2, &[shallow])])),
        )
        .compile()
        .unwrap();
        assert_eq!(table_only.cap_for(&shallow), None);
        assert_eq!(table_only.cap_for(&unlisted), None);
    }

    #[test]
    fn single_contract_over_its_depth_cap_is_a_transaction_scope_overrun() {
        let xen_like = Address::repeat_byte(0xee);
        let limits = ContractSlotLimits::new(None, Some(depth_table(2_100, &[(8, &[xen_like])])))
            .compile()
            .unwrap();
        let mut usage = ContractSlotUsage::default();

        usage.stage(&state_with_changed_slots(&[(xen_like, 380)]));
        assert_eq!(limits.check(&usage), None);

        usage.stage(&state_with_changed_slots(&[
            (Address::repeat_byte(0x01), 900),
            (xen_like, 403),
        ]));
        assert_eq!(
            limits.check(&usage),
            Some(ContractSlotLimitExceeded {
                owner: xen_like,
                depth: Some(8),
                scope: ResourceThrottlingLimitScope::Transaction,
                used: 403,
                transaction_slots: 403,
                limit: 380,
            })
        );
    }

    #[test]
    fn committed_counts_accumulate_per_owner_and_ignore_uncommitted_attempts() {
        let owner = Address::repeat_byte(0xaa);
        let other = Address::repeat_byte(0xbb);
        let limits = ContractSlotLimits::new(Some(500), None).compile().unwrap();
        let mut usage = ContractSlotUsage::default();

        usage.stage(&state_with_changed_slots(&[(owner, 300), (other, 10)]));
        usage.commit().unwrap();
        // Executed but not committed: must not reach the totals.
        usage.stage(&state_with_changed_slots(&[(owner, 400)]));
        usage.stage(&state_with_changed_slots(&[(owner, 150)]));
        assert_eq!(limits.check(&usage), None);
        usage.commit().unwrap();

        assert_eq!(usage.committed.get(&owner), Some(&450));
        assert_eq!(usage.committed.get(&other), Some(&10));
        assert!(usage.staged.is_empty());

        usage.stage(&state_with_changed_slots(&[(owner, 100)]));
        assert_eq!(
            limits.check(&usage),
            Some(ContractSlotLimitExceeded {
                owner,
                depth: None,
                scope: ResourceThrottlingLimitScope::Block,
                used: 550,
                transaction_slots: 100,
                limit: 500,
            })
        );
        // A commit without a new stage adds nothing.
        usage.commit().unwrap();
        usage.commit().unwrap();
        assert_eq!(usage.committed.get(&owner), Some(&550));
    }

    #[test]
    fn commit_overflow_saturates_and_reports() {
        let owner = Address::repeat_byte(0xaa);
        let mut usage = ContractSlotUsage::default();
        usage.committed.insert(owner, u64::MAX);
        usage.stage(&state_with_changed_slots(&[(owner, 1)]));
        assert_eq!(usage.commit(), Err(ResourceMeteringError::ArithmeticOverflow));
        assert_eq!(usage.committed.get(&owner), Some(&u64::MAX));
    }

    #[test]
    fn rejects_invalid_limits() {
        let owner = Address::repeat_byte(0xaa);
        let enforced =
            ContractSlotLimits { dry_run: false, ..ContractSlotLimits::new(Some(1), None) };
        assert_eq!(enforced.compile(), Err(ResourceMeteringError::ContractSlotsNotDryRun));
        assert_eq!(
            ContractSlotLimits::new(None, None).compile(),
            Err(ResourceMeteringError::EmptyContractSlotLimits)
        );
        assert_eq!(
            ContractSlotLimits::new(Some(0), None).compile(),
            Err(ResourceMeteringError::ZeroContractSlotLimit)
        );
        assert_eq!(
            ContractSlotLimits::new(None, Some(depth_table(0, &[(8, &[owner])]))).compile(),
            Err(ResourceMeteringError::ZeroContractSlotLimit)
        );
        for depth in [0, MAX_TRIE_DEPTH + 1] {
            assert_eq!(
                ContractSlotLimits::new(None, Some(depth_table(2_100, &[(depth, &[owner])])))
                    .compile(),
                Err(ResourceMeteringError::InvalidTrieDepth(depth))
            );
        }
        assert_eq!(
            ContractSlotLimits::new(
                None,
                Some(depth_table(2_100, &[(8, &[owner]), (7, &[owner])]))
            )
            .compile(),
            Err(ResourceMeteringError::DuplicateTrieDepthContract(owner))
        );
    }
}
