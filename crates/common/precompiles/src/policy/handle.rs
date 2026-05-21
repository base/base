//! Business logic for the `PolicyRegistry` precompile.
//!
//! [`PolicyHandle`] is the concrete type the token holds. It wraps [`PolicyRegistryStorage`]
//! and implements [`Policy`] (for authorization checks) and [`PolicyRegistry`] (for admin ops).

use alloc::vec::Vec;
use core::fmt;

use alloy_primitives::Address;
use base_precompile_storage::{Result, StorageCtx};

use super::storage::PolicyRegistryStorage;
use crate::{IPolicyRegistry::PolicyType, Policy, PolicyRegistry};

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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{IPolicyRegistry, Policy, PolicyRegistry};

    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");

    fn storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        s
    }

    #[test]
    fn policy_trait_is_authorized_builtin_ids() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert!(handle.is_authorized(PolicyRegistryStorage::ALWAYS_ALLOW_ID, ALICE).unwrap());
            assert!(!handle.is_authorized(PolicyRegistryStorage::ALWAYS_BLOCK_ID, ALICE).unwrap());
        });
    }

    #[test]
    fn policy_registry_trait_create_and_authorize() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyHandle::new(ctx).create_policy(ADMIN, IPolicyRegistry::PolicyType::ALLOWLIST)
        })
        .unwrap();

        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyHandle::new(ctx).update_allowlist(id, true, alloc::vec![ALICE])
        })
        .unwrap();

        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert!(handle.is_authorized(id, ALICE).unwrap());
        });
    }

    #[test]
    fn policy_registry_trait_next_policy_id() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            let id = handle.next_policy_id(IPolicyRegistry::PolicyType::ALLOWLIST).unwrap();
            assert_eq!((id >> 56) as u8, IPolicyRegistry::PolicyType::ALLOWLIST as u8);
        });
    }

    #[test]
    fn policy_registry_trait_policy_exists() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert!(handle.policy_exists(PolicyRegistryStorage::ALWAYS_ALLOW_ID).unwrap());
            assert!(handle.policy_exists(PolicyRegistryStorage::ALWAYS_BLOCK_ID).unwrap());
            assert!(!handle.policy_exists(0xdeadbeef).unwrap());
        });
    }

    #[test]
    fn policy_registry_trait_admin_transfer() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyHandle::new(ctx).create_policy(ADMIN, IPolicyRegistry::PolicyType::BLOCKLIST)
        })
        .unwrap();

        StorageCtx::enter(&mut s, |ctx| PolicyHandle::new(ctx).stage_update_admin(id, NEW_ADMIN))
            .unwrap();

        s.set_caller(NEW_ADMIN);
        StorageCtx::enter(&mut s, |ctx| PolicyHandle::new(ctx).finalize_update_admin(id)).unwrap();

        StorageCtx::enter(&mut s, |ctx| {
            assert_eq!(PolicyHandle::new(ctx).get_policy_admin(id).unwrap(), NEW_ADMIN);
        });
    }

    #[test]
    fn policy_registry_trait_get_policy_type() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert_eq!(
                handle.get_policy_type(PolicyRegistryStorage::ALWAYS_ALLOW_ID).unwrap(),
                IPolicyRegistry::PolicyType::ALWAYS_ALLOW
            );
            assert_eq!(
                handle.get_policy_type(PolicyRegistryStorage::ALWAYS_BLOCK_ID).unwrap(),
                IPolicyRegistry::PolicyType::ALWAYS_BLOCK
            );
        });
    }
}
