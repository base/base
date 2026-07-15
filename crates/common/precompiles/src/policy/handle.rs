//! Consumer-facing handle for the `PolicyRegistry` precompile.
//!
//! [`PolicyHandle`] is the concrete type B-20 tokens hold as their `Policy` source. It
//! binds the registry storage into a [`PolicyRegistryRuntime`] and implements the outward
//! [`Policy`] (authorization reads) and [`PolicyRegistry`] (admin ops) traits by delegating
//! to the frozen [`PolicyRegistryV1`] logic.

use alloc::vec::Vec;
use core::fmt;

use alloy_primitives::Address;
use base_precompile_storage::{Result, StorageCtx};

use crate::{
    IPolicyRegistry::PolicyType, Policy, PolicyRegistry, PolicyRegistryLogic,
    PolicyRegistryRuntime, PolicyRegistryStorage, PolicyRegistryV1,
};

/// Binds [`PolicyRegistryStorage`] into a runtime and exposes the outward [`Policy`] and
/// [`PolicyRegistry`] traits, delegating to the version-frozen [`PolicyRegistryV1`] logic.
///
// TODO: Pin to V1 until the consumer call path (B20Guards reads, factory/admin setup)
// threads the active fork. Once it does, resolve the version via
// `crate::PolicyVersions::from_base_upgrade` instead of hard-coding `PolicyRegistryV1`,
// mirroring the fork-threaded token dispatchers.
pub struct PolicyHandle<'a> {
    runtime: PolicyRegistryRuntime<PolicyRegistryStorage<'a>>,
}

impl<'a> PolicyHandle<'a> {
    /// Creates a `PolicyHandle` backed by the registry storage at its singleton address.
    pub fn new(ctx: StorageCtx<'a>) -> Self {
        Self { runtime: PolicyRegistryRuntime::with_storage(PolicyRegistryStorage::new(ctx)) }
    }
}

impl fmt::Debug for PolicyHandle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyHandle").finish_non_exhaustive()
    }
}

impl Policy for PolicyHandle<'_> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        PolicyRegistryV1.is_authorized(&self.runtime, policy_id, account)
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        PolicyRegistryV1.policy_exists(&self.runtime, policy_id)
    }
}

impl PolicyRegistry for PolicyHandle<'_> {
    fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64> {
        PolicyRegistryV1.create_policy(&mut self.runtime, admin, policy_type)
    }

    fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        PolicyRegistryV1.create_policy_with_accounts(
            &mut self.runtime,
            admin,
            policy_type,
            accounts,
        )
    }

    fn stage_update_admin(&mut self, policy_id: u64, new_admin: Address) -> Result<()> {
        PolicyRegistryV1.stage_update_admin(&mut self.runtime, policy_id, new_admin)
    }

    fn finalize_update_admin(&mut self, policy_id: u64) -> Result<()> {
        PolicyRegistryV1.finalize_update_admin(&mut self.runtime, policy_id)
    }

    fn renounce_admin(&mut self, policy_id: u64) -> Result<()> {
        PolicyRegistryV1.renounce_admin(&mut self.runtime, policy_id)
    }

    fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        PolicyRegistryV1.update_allowlist(&mut self.runtime, policy_id, allowed, accounts)
    }

    fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        PolicyRegistryV1.update_blocklist(&mut self.runtime, policy_id, blocked, accounts)
    }

    fn get_policy_admin(&self, policy_id: u64) -> Result<Address> {
        PolicyRegistryV1.get_policy_admin(&self.runtime, policy_id)
    }

    fn pending_policy_admin(&self, policy_id: u64) -> Result<Address> {
        PolicyRegistryV1.pending_policy_admin(&self.runtime, policy_id)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{
        IPolicyRegistry, Policy, PolicyHandle, PolicyRegistry, PolicyRegistryRuntime,
        PolicyRegistryStorage, PolicyRegistryV1,
    };

    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");

    /// Storage with both built-in policies pre-seeded (via the pinned V1 bootstrap).
    fn storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            let mut rt = PolicyRegistryRuntime::with_storage(PolicyRegistryStorage::new(ctx));
            PolicyRegistryV1.ensure_initialized_and_get_counter(&mut rt)
        })
        .unwrap();
        s
    }

    #[test]
    fn policy_trait_is_authorized_builtin_ids() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert!(handle.is_authorized(PolicyRegistryV1::ALWAYS_ALLOW_ID, ALICE).unwrap());
            assert!(!handle.is_authorized(PolicyRegistryV1::ALWAYS_BLOCK_ID, ALICE).unwrap());
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
    fn policy_registry_trait_policy_exists() {
        let mut s = storage();
        StorageCtx::enter(&mut s, |ctx| {
            let handle = PolicyHandle::new(ctx);
            assert!(handle.policy_exists(PolicyRegistryV1::ALWAYS_ALLOW_ID).unwrap());
            assert!(handle.policy_exists(PolicyRegistryV1::ALWAYS_BLOCK_ID).unwrap());
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
}
