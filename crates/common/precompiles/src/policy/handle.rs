//! Business logic for the `PolicyRegistry` precompile.
//!
//! [`PolicyHandle`] is the concrete type the token holds. It wraps [`PolicyRegistryStorage`]
//! and implements [`Policy`] (for authorization checks) and [`PolicyRegistry`] (for admin ops).

use alloc::vec::Vec;
use core::fmt;

use alloy_primitives::Address;
use base_precompile_storage::{Result, StorageCtx};

use super::storage::PolicyRegistryStorage;
use crate::{Policy, PolicyRegistry, PolicyType};

/// Wraps [`PolicyRegistryStorage`] and implements [`Policy`] and [`PolicyRegistry`],
/// separating authorization decisions from raw storage reads.
pub struct PolicyHandle<'a> {
    inner: PolicyRegistryStorage<'a>,
}

impl<'a> PolicyHandle<'a> {
    /// Creates a `PolicyHandle` backed by the registry storage at its singleton address.
    pub fn new(ctx: StorageCtx<'a>) -> Self {
        Self { inner: PolicyRegistryStorage::new(ctx) }
    }
}

impl fmt::Debug for PolicyHandle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyHandle").finish_non_exhaustive()
    }
}

impl Policy for PolicyHandle<'_> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.inner.is_authorized(policy_id, account)
    }
}

impl PolicyRegistry for PolicyHandle<'_> {
    fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64> {
        self.inner.create_policy(admin, policy_type)
    }

    fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        self.inner.create_policy_with_accounts(admin, policy_type, accounts)
    }

    fn stage_update_admin(&mut self, policy_id: u64, new_admin: Address) -> Result<()> {
        self.inner.stage_update_admin(policy_id, new_admin)
    }

    fn finalize_update_admin(&mut self, policy_id: u64) -> Result<()> {
        self.inner.finalize_update_admin(policy_id)
    }

    fn renounce_admin(&mut self, policy_id: u64) -> Result<()> {
        self.inner.renounce_admin(policy_id)
    }

    fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        self.inner.update_allowlist(policy_id, allowed, accounts)
    }

    fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        self.inner.update_blocklist(policy_id, blocked, accounts)
    }

    fn next_policy_id(&self, policy_type: PolicyType) -> Result<u64> {
        self.inner.next_policy_id(policy_type)
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        self.inner.policy_exists(policy_id)
    }

    fn get_policy_type(&self, policy_id: u64) -> Result<PolicyType> {
        self.inner.get_policy_type(policy_id)
    }

    fn get_policy_admin(&self, policy_id: u64) -> Result<Address> {
        self.inner.get_policy_admin(policy_id)
    }

    fn pending_policy_admin(&self, policy_id: u64) -> Result<Address> {
        self.inner.pending_policy_admin(policy_id)
    }
}
