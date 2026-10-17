//! Access policy hook (design §6): principal authorization, work quotas and charges.

use std::{collections::HashMap, fmt::Debug};

use alloy_primitives::{Address, B256, TxHash, U256};
use parking_lot::Mutex;
use serde::Deserialize;

use crate::CandidateError;

/// Identity that owns quota; every sender of one principal shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
pub struct PrincipalId(pub u64);

/// The parts of a group the access policy prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSummary {
    /// Group identity.
    pub group_hash: B256,
    /// Shared sender.
    pub sender: Address,
    /// Signed lifetime work-gas cap.
    pub max_work_gas: u64,
    /// Signed lifetime evaluation cap.
    pub max_evaluations: u16,
    /// Signed upper bound on any charge, in wei.
    pub max_access_charge: U256,
}

/// Identity of one evaluation round, used to deduplicate work records and settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptId {
    /// Group identity.
    pub group_hash: B256,
    /// Parent of the block being built.
    pub parent_hash: B256,
    /// The group's consumed-evaluation count including this round, starting at 1. Not unique:
    /// an interrupted round returns its attempt and the next round reuses the number.
    pub seq: u16,
    /// Identity of the turn that ran this round, from [`crate::GroupLedger::next_sequence`].
    /// Never reused by one ledger; pair it with [`crate::GroupLedger::instance`] across
    /// processes.
    pub turn: u64,
}

/// Why the policy refused admission or work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccessDenied {
    /// The sender maps to no principal, or the principal was removed.
    #[error("principal not allowed")]
    NotAllowed,
    /// The policy price for the declared budget exceeds `maxAccessCharge`.
    #[error("access charge too low")]
    AccessChargeTooLow,
    /// The principal's work quota cannot cover the request.
    #[error("principal quota exhausted")]
    QuotaExhausted,
}

impl From<AccessDenied> for CandidateError {
    fn from(denied: AccessDenied) -> Self {
        match denied {
            AccessDenied::NotAllowed => Self::PrincipalNotAllowed,
            AccessDenied::AccessChargeTooLow => Self::AccessChargeTooLow,
            AccessDenied::QuotaExhausted => Self::PrincipalQuotaExhausted,
        }
    }
}

/// Principal work reserved for one round; settle it exactly once.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkTicket {
    /// Principal charged.
    pub principal: PrincipalId,
    /// Round the ticket belongs to.
    pub attempt: AttemptId,
    /// Gas reserved before dispatch.
    pub reserved_gas: u64,
}

/// How a group left the pool, for charge finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOutcome {
    /// A selected winner landed.
    Landed {
        /// Winning sibling.
        winner: TxHash,
    },
    /// The default sibling landed through fallback.
    Fallback {
        /// The default sibling.
        winner: TxHash,
    },
    /// The group was evicted or expired without landing.
    Unlanded,
}

/// Charge computed when a group finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeDecision {
    /// What the planned formula would charge, capped at `maxAccessCharge`.
    pub would_charge: U256,
    /// What is actually charged. Zero during the beta.
    pub charged: U256,
}

/// Access-control hook consulted at admission and before every evaluation round.
pub trait CandidateAccessPolicy: Send + Sync + Debug {
    /// Returns the principal currently authorized for `sender`.
    fn principal(&self, sender: Address) -> Option<PrincipalId>;

    /// Returns the registry version that ledger entries bind.
    fn policy_version(&self) -> u64;

    /// Admission: authorization, quota headroom, and price for the declared budget.
    fn admit(&self, principal: PrincipalId, group: &GroupSummary) -> Result<(), AccessDenied>;

    /// Atomically checks authorization and reserves principal work before a round.
    fn reserve(
        &self,
        principal: PrincipalId,
        attempt: AttemptId,
        gas: u64,
    ) -> Result<WorkTicket, AccessDenied>;

    /// Settles a ticket at actual work and releases the unused reservation.
    fn settle(&self, ticket: WorkTicket, gas_used: u64);

    /// Computes the (beta: would-)charge once a group finishes.
    fn finalize(&self, principal: PrincipalId, outcome: GroupOutcome) -> ChargeDecision;
}

/// One principal entry in a principals file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrincipalEntry {
    /// Principal identity.
    pub id: PrincipalId,
    /// Senders that act for the principal.
    pub senders: Vec<Address>,
}

/// JSON principals file loaded by `--txpool.candidates.principals-file`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrincipalsFile {
    /// Registry version bound into every ledger entry.
    pub policy_version: u64,
    /// Per-principal work-gas quota for the life of the process; `None` is unlimited.
    #[serde(default)]
    pub work_gas_quota: Option<u64>,
    /// Allowlisted principals.
    pub principals: Vec<PrincipalEntry>,
}

/// Allowlisted zero-charge beta policy (design §6.2).
///
/// An empty registry admits nobody. Quota is a lifetime work-gas total per principal shared by
/// all of its senders; reservations count against it until settled.
#[derive(Debug, Default)]
pub struct StaticAccessPolicy {
    senders: HashMap<Address, PrincipalId>,
    policy_version: u64,
    work_gas_quota: Option<u64>,
    used: Mutex<HashMap<PrincipalId, u64>>,
}

impl StaticAccessPolicy {
    /// Builds a policy from a parsed principals file.
    pub fn new(file: PrincipalsFile) -> Self {
        let senders = file
            .principals
            .iter()
            .flat_map(|entry| entry.senders.iter().map(|sender| (*sender, entry.id)))
            .collect();
        Self {
            senders,
            policy_version: file.policy_version,
            work_gas_quota: file.work_gas_quota,
            used: Mutex::default(),
        }
    }

    /// Returns the work gas reserved or settled for `principal`.
    pub fn used_work_gas(&self, principal: PrincipalId) -> u64 {
        self.used.lock().get(&principal).copied().unwrap_or_default()
    }

    /// Returns whether `principal` is still listed in the principals file.
    pub fn authorized(&self, principal: PrincipalId) -> bool {
        self.senders.values().any(|id| *id == principal)
    }
}

impl CandidateAccessPolicy for StaticAccessPolicy {
    fn principal(&self, sender: Address) -> Option<PrincipalId> {
        self.senders.get(&sender).copied()
    }

    fn policy_version(&self) -> u64 {
        self.policy_version
    }

    fn admit(&self, principal: PrincipalId, _group: &GroupSummary) -> Result<(), AccessDenied> {
        if !self.authorized(principal) {
            return Err(AccessDenied::NotAllowed);
        }
        // Pricing is TBD (design §6); the beta price is zero, so any `maxAccessCharge` covers it.
        if let Some(quota) = self.work_gas_quota
            && self.used_work_gas(principal) >= quota
        {
            return Err(AccessDenied::QuotaExhausted);
        }
        Ok(())
    }

    fn reserve(
        &self,
        principal: PrincipalId,
        attempt: AttemptId,
        gas: u64,
    ) -> Result<WorkTicket, AccessDenied> {
        if !self.authorized(principal) {
            return Err(AccessDenied::NotAllowed);
        }
        let mut used = self.used.lock();
        let entry = used.entry(principal).or_default();
        let next = entry.saturating_add(gas);
        if self.work_gas_quota.is_some_and(|quota| next > quota) {
            return Err(AccessDenied::QuotaExhausted);
        }
        *entry = next;
        Ok(WorkTicket { principal, attempt, reserved_gas: gas })
    }

    fn settle(&self, ticket: WorkTicket, gas_used: u64) {
        let refund = ticket.reserved_gas.saturating_sub(gas_used);
        if let Some(entry) = self.used.lock().get_mut(&ticket.principal) {
            *entry = entry.saturating_sub(refund);
        }
    }

    fn finalize(&self, _principal: PrincipalId, _outcome: GroupOutcome) -> ChargeDecision {
        ChargeDecision { would_charge: U256::ZERO, charged: U256::ZERO }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(quota: Option<u64>) -> StaticAccessPolicy {
        StaticAccessPolicy::new(PrincipalsFile {
            policy_version: 3,
            work_gas_quota: quota,
            principals: vec![PrincipalEntry {
                id: PrincipalId(1),
                senders: vec![Address::repeat_byte(1), Address::repeat_byte(2)],
            }],
        })
    }

    fn summary() -> GroupSummary {
        GroupSummary {
            group_hash: B256::ZERO,
            sender: Address::repeat_byte(1),
            max_work_gas: 1,
            max_evaluations: 1,
            max_access_charge: U256::ZERO,
        }
    }

    fn attempt() -> AttemptId {
        AttemptId { group_hash: B256::ZERO, parent_hash: B256::ZERO, seq: 1, turn: 1 }
    }

    #[test]
    fn empty_registry_admits_nobody() {
        let policy = StaticAccessPolicy::default();
        assert_eq!(policy.principal(Address::repeat_byte(1)), None);
        assert_eq!(policy.admit(PrincipalId(1), &summary()), Err(AccessDenied::NotAllowed));
    }

    #[test]
    fn alias_senders_share_one_principal_quota() {
        let policy = policy(Some(100));
        let principal = policy.principal(Address::repeat_byte(1)).unwrap();
        assert_eq!(policy.principal(Address::repeat_byte(2)), Some(principal));
        assert_eq!(policy.policy_version(), 3);

        let ticket = policy.reserve(principal, attempt(), 60).unwrap();
        assert_eq!(policy.reserve(principal, attempt(), 60), Err(AccessDenied::QuotaExhausted));
        policy.settle(ticket, 20);
        assert_eq!(policy.used_work_gas(principal), 20);
        policy.reserve(principal, attempt(), 80).unwrap();
        assert_eq!(policy.admit(principal, &summary()), Err(AccessDenied::QuotaExhausted));
    }

    #[test]
    fn unknown_principal_cannot_reserve() {
        assert_eq!(
            policy(None).reserve(PrincipalId(9), attempt(), 1),
            Err(AccessDenied::NotAllowed)
        );
    }

    #[test]
    fn principals_file_parses_and_rejects_unknown_fields() {
        let json = r#"{"policyVersion":2,"workGasQuota":5,"principals":[{"id":7,"senders":["0x0101010101010101010101010101010101010101"]}]}"#;
        let file: PrincipalsFile = serde_json::from_str(json).unwrap();
        let policy = StaticAccessPolicy::new(file);
        assert_eq!(policy.principal(Address::repeat_byte(1)), Some(PrincipalId(7)));
        assert!(
            serde_json::from_str::<PrincipalsFile>(r#"{"policyVersion":1,"principals":[],"x":1}"#)
                .is_err()
        );
    }

    #[test]
    fn beta_never_charges() {
        let decision = policy(None).finalize(PrincipalId(1), GroupOutcome::Unlanded);
        assert_eq!(decision.charged, U256::ZERO);
    }
}
