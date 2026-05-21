use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    abi::{IPolicyRegistry, IPolicyRegistry::IPolicyRegistryCalls as C},
    storage::PolicyRegistryStorage,
};
use crate::{
    ActivationFeature, ActivationRegistryStorage,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl PolicyRegistryStorage<'_> {
    pub(super) fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationFeature::PolicyRegistry.id())
            .and_then(|()| self.inner(calldata))
            .into_precompile_result(ctx.gas_used(), |b| b)
    }

    fn inner(&mut self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, IPolicyRegistry::IPolicyRegistryCalls) {
            C::createPolicy(call) => {
                let id = self.create_policy(call.admin, call.policyType)?;
                Ok(IPolicyRegistry::createPolicyCall::abi_encode_returns(&id).into())
            }
            C::createPolicyWithAccounts(call) => {
                let id =
                    self.create_policy_with_accounts(call.admin, call.policyType, call.accounts)?;
                Ok(IPolicyRegistry::createPolicyWithAccountsCall::abi_encode_returns(&id).into())
            }
            C::stageUpdateAdmin(call) => {
                self.stage_update_admin(call.policyId, call.newAdmin)?;
                Ok(Bytes::new())
            }
            C::finalizeUpdateAdmin(call) => {
                self.finalize_update_admin(call.policyId)?;
                Ok(Bytes::new())
            }
            C::renounceAdmin(call) => {
                self.renounce_admin(call.policyId)?;
                Ok(Bytes::new())
            }
            C::updateAllowlist(call) => {
                self.update_allowlist(call.policyId, call.allowed, call.accounts)?;
                Ok(Bytes::new())
            }
            C::updateBlocklist(call) => {
                self.update_blocklist(call.policyId, call.blocked, call.accounts)?;
                Ok(Bytes::new())
            }
            C::isAuthorized(call) => {
                let authorized = self.is_authorized(call.policyId, call.account)?;
                Ok(IPolicyRegistry::isAuthorizedCall::abi_encode_returns(&authorized).into())
            }
            C::policyExists(call) => {
                let exists = self.policy_exists(call.policyId)?;
                Ok(IPolicyRegistry::policyExistsCall::abi_encode_returns(&exists).into())
            }
            C::policyType(call) => {
                let pt = self.get_policy_type(call.policyId)?;
                Ok(IPolicyRegistry::policyTypeCall::abi_encode_returns(&pt).into())
            }
            C::policyAdmin(call) => {
                let admin = self.get_policy_admin(call.policyId)?;
                Ok(IPolicyRegistry::policyAdminCall::abi_encode_returns(&admin).into())
            }
            C::pendingPolicyAdmin(call) => {
                let pending = self.pending_policy_admin(call.policyId)?;
                Ok(IPolicyRegistry::pendingPolicyAdminCall::abi_encode_returns(&pending).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{
        ActivationFeature, ActivationRegistryStorage, IPolicyRegistry, PolicyRegistryStorage,
    };

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");

    fn activate_policy_registry(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationFeature::PolicyRegistry.id(), Some(ACTIVATION_ADMIN))
                .unwrap()
        });
    }

    #[test]
    fn dispatch_reverts_when_policy_registry_is_inactive() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(output.reverted);
    }

    #[test]
    fn dispatch_succeeds_when_policy_registry_is_active() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!output.reverted);
        assert!(IPolicyRegistry::policyExistsCall::abi_decode_returns(&output.bytes).unwrap());
    }

    #[test]
    fn dispatch_create_policy_returns_policy_id() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        storage.set_caller(ADMIN);
        let calldata = IPolicyRegistry::createPolicyCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!output.reverted);
        let id = IPolicyRegistry::createPolicyCall::abi_decode_returns(&output.bytes).unwrap();
        assert_eq!((id >> 56) as u8, IPolicyRegistry::PolicyType::ALLOWLIST as u8);
    }

    #[test]
    fn dispatch_is_authorized_always_allow_returns_true() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let calldata = IPolicyRegistry::isAuthorizedCall {
            policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID,
            account: ALICE,
        }
        .abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!output.reverted);
        assert!(IPolicyRegistry::isAuthorizedCall::abi_decode_returns(&output.bytes).unwrap());
    }

    #[test]
    fn dispatch_unknown_selector_reverts() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let calldata = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x00, 0x00, 0x00];

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(output.reverted);
    }

    fn create_allowlist_policy(storage: &mut HashMapStorageProvider) -> u64 {
        storage.set_caller(ADMIN);
        let calldata = IPolicyRegistry::createPolicyCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .abi_encode();
        let output = StorageCtx::enter(storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!output.reverted, "create_allowlist_policy setup unexpectedly reverted");
        IPolicyRegistry::createPolicyCall::abi_decode_returns(&output.bytes).unwrap()
    }

    #[test]
    fn dispatch_create_policy_with_accounts() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        storage.set_caller(ADMIN);
        let calldata = IPolicyRegistry::createPolicyWithAccountsCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            accounts: alloc::vec![ALICE],
        }
        .abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();

        assert!(!output.reverted);
        let id = IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(&output.bytes)
            .unwrap();
        assert_eq!((id >> 56) as u8, IPolicyRegistry::PolicyType::ALLOWLIST as u8);
    }

    #[test]
    fn dispatch_stage_and_finalize_update_admin() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let id = create_allowlist_policy(&mut storage);
        let new_admin = address!("0x3000000000000000000000000000000000000003");

        // stage
        storage.set_caller(ADMIN);
        let stage_calldata =
            IPolicyRegistry::stageUpdateAdminCall { policyId: id, newAdmin: new_admin }
                .abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &stage_calldata)
        })
        .unwrap();
        assert!(!out.reverted);

        // finalize
        storage.set_caller(new_admin);
        let finalize_calldata =
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: id }.abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &finalize_calldata)
        })
        .unwrap();
        assert!(!out.reverted);

        // confirm admin changed
        let admin_calldata = IPolicyRegistry::policyAdminCall { policyId: id }.abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &admin_calldata)
        })
        .unwrap();
        let admin = IPolicyRegistry::policyAdminCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(admin, new_admin);
    }

    #[test]
    fn dispatch_renounce_admin() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let id = create_allowlist_policy(&mut storage);

        storage.set_caller(ADMIN);
        let calldata = IPolicyRegistry::renounceAdminCall { policyId: id }.abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!out.reverted);
    }

    #[test]
    fn dispatch_update_allowlist_and_blocklist() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let id = create_allowlist_policy(&mut storage);

        storage.set_caller(ADMIN);
        let calldata = IPolicyRegistry::updateAllowlistCall {
            policyId: id,
            allowed: true,
            accounts: alloc::vec![ALICE],
        }
        .abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!out.reverted);

        // updateBlocklist on a blocklist policy
        storage.set_caller(ADMIN);
        let blocklist_calldata = IPolicyRegistry::createPolicyCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
        }
        .abi_encode();
        let blocklist_out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &blocklist_calldata)
        })
        .unwrap();
        assert!(!blocklist_out.reverted, "blocklist policy creation unexpectedly reverted");
        let bid =
            IPolicyRegistry::createPolicyCall::abi_decode_returns(&blocklist_out.bytes).unwrap();

        storage.set_caller(ADMIN);
        let update_blocklist = IPolicyRegistry::updateBlocklistCall {
            policyId: bid,
            blocked: true,
            accounts: alloc::vec![ALICE],
        }
        .abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &update_blocklist)
        })
        .unwrap();
        assert!(!out.reverted);
    }

    #[test]
    fn dispatch_policy_type() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);

        let calldata =
            IPolicyRegistry::policyTypeCall { policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID }
                .abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!out.reverted);
        let pt = IPolicyRegistry::policyTypeCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(pt, IPolicyRegistry::PolicyType::ALWAYS_ALLOW);
    }

    #[test]
    fn dispatch_pending_policy_admin() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let id = create_allowlist_policy(&mut storage);

        let calldata = IPolicyRegistry::pendingPolicyAdminCall { policyId: id }.abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!out.reverted);
        let pending =
            IPolicyRegistry::pendingPolicyAdminCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(pending, Address::ZERO);
    }
}
