//! Policy traits — the outward-facing interfaces tokens and callers use for the policy registry.

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::IPolicyRegistry::PolicyType;

/// Minimal read-only policy interface consulted by B-20 tokens on every transfer and mint.
///
/// # `is_authorized` vs `policy_exists`
///
/// These two methods can diverge for never-created BLOCKLIST IDs: `policy_exists` returns `false`
/// (the slot was never written) while `is_authorized` returns `true` (empty blocklist allows
/// everyone). Do not gate `is_authorized` calls on a prior `policy_exists` check — call
/// `is_authorized` directly; it handles all cases correctly on its own.
pub trait Policy {
    /// Returns `true` if `account` is authorized under the given `policy_id`.
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool>;

    /// Returns `true` if `policy_id` is a built-in or previously created policy.
    fn policy_exists(&self, policy_id: u64) -> Result<bool>;
}

/// Full policy registry interface including administrative mutations.
///
/// Extends [`Policy`] so any `PolicyRegistry` implementor also satisfies the minimal token bound.
pub trait PolicyRegistry: Policy {
    /// Creates a new ALLOWLIST or BLOCKLIST policy, returning its encoded ID.
    fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64>;
    /// Creates a new policy and seeds it with an initial member list.
    fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: PolicyType,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<u64>;
    /// Stages a pending admin transfer for `policy_id`.
    /// Pass `Address::ZERO` to clear a previously staged transfer without nominating a replacement.
    fn stage_update_admin(&mut self, policy_id: u64, new_admin: Address) -> Result<()>;
    /// Completes a pending admin transfer; caller must be the staged pending admin.
    fn finalize_update_admin(&mut self, policy_id: u64) -> Result<()>;
    /// Permanently relinquishes admin of `policy_id`.
    fn renounce_admin(&mut self, policy_id: u64) -> Result<()>;
    /// Adds or removes accounts from an ALLOWLIST policy's member set.
    fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<()>;
    /// Adds or removes accounts from a BLOCKLIST policy's member set.
    fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<()>;
    /// Returns the current admin of `policy_id`, or `address(0)` if the policy does not exist
    /// or the policy ID is malformed. Never reverts.
    fn get_policy_admin(&self, policy_id: u64) -> Result<Address>;
    /// Returns the staged pending admin for `policy_id`, or `address(0)` if none, the policy
    /// does not exist, or the policy ID is malformed. Never reverts.
    fn pending_policy_admin(&self, policy_id: u64) -> Result<Address>;
}
