//! Append-only business-logic interface for the `PolicyRegistry` precompile.

use alloc::vec::Vec;

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{IPolicyRegistry::PolicyType, PolicyAccounting};

/// The policy-registry logic interface.
///
/// Each method takes the [`PolicyAccounting`] storage port directly. Versioned
/// implementations are resolved via [`crate::PolicyVersions`].
pub trait PolicyRegistryLogic<S: PolicyAccounting> {
    /// Creates a new ALLOWLIST or BLOCKLIST policy, returning its encoded ID.
    fn create_policy(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
    ) -> Result<u64>;

    /// Creates a new policy and seeds it with an initial member list.
    fn create_policy_with_accounts(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64>;

    /// Stages a pending admin transfer for `policy_id`.
    ///
    /// Passing `Address::ZERO` clears a previously staged transfer without nominating a
    /// replacement.
    fn stage_update_admin(&self, storage: &mut S, policy_id: u64, new_admin: Address)
    -> Result<()>;

    /// Completes a pending admin transfer; caller must be the staged pending admin.
    fn finalize_update_admin(&self, storage: &mut S, policy_id: u64) -> Result<()>;

    /// Permanently relinquishes admin of `policy_id`.
    fn renounce_admin(&self, storage: &mut S, policy_id: u64) -> Result<()>;

    /// Adds or removes `accounts` from an ALLOWLIST policy's member set.
    fn update_allowlist(
        &self,
        storage: &mut S,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()>;

    /// Adds or removes `accounts` from a BLOCKLIST policy's member set.
    fn update_blocklist(
        &self,
        storage: &mut S,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()>;

    /// Returns whether `account` is authorized under `policy_id`.
    fn is_authorized(&self, storage: &S, policy_id: u64, account: Address) -> Result<bool>;

    /// Returns whether `policy_id` refers to a built-in or previously created policy.
    fn policy_exists(&self, storage: &S, policy_id: u64) -> Result<bool>;

    /// Returns the current admin of `policy_id`, or `Address::ZERO` if none / malformed.
    fn get_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address>;

    /// Returns the staged pending admin for `policy_id`, or `Address::ZERO` if none.
    fn pending_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address>;
}
