use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    abi::{IPolicyRegistry, IPolicyRegistry::IPolicyRegistryCalls as C},
    storage::PolicyRegistryStorage,
};
use crate::{ActivationRegistryStorage, macros::decode_precompile_call};

impl PolicyRegistryStorage<'_> {
    pub(super) fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        if let Err(e) = ctx.deduct_gas(crate::input_cost(calldata.len())) {
            return e.into_precompile_result(ctx.gas_used());
        }
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationRegistryStorage::POLICY_REGISTRY)
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
            C::nextPolicyId(call) => {
                let id = self.next_policy_id(call.policyType)?;
                Ok(IPolicyRegistry::nextPolicyIdCall::abi_encode_returns(&id).into())
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

    use super::*;
    use crate::{ActivationRegistryStorage, IPolicyRegistry};

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");

    fn activate_policy_registry(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationRegistryStorage::POLICY_REGISTRY, Some(ACTIVATION_ADMIN))
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
}
