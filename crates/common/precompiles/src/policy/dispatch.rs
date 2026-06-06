use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::StorageCtx;
use revm::precompile::PrecompileResult;

use crate::{
    ActivationFeature, ActivationRegistryStorage,
    IPolicyRegistry::{self, IPolicyRegistryCalls as C},
    PolicyRegistryStorage,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl PolicyRegistryStorage<'_> {
    /// ABI-dispatches policy registry calldata.
    ///
    /// View (read-only) calls bypass the activation gate and remain accessible even when the
    /// feature is disabled. Write calls require the feature to be activated.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        let result = if Self::is_view_selector(calldata) {
            self.inner(calldata)
        } else {
            ActivationRegistryStorage::new(ctx)
                .ensure_activated(ActivationFeature::PolicyRegistry.id())
                .and_then(|()| self.inner(calldata))
        };
        ctx.result_output(result, |b| b)
    }

    /// Returns `true` when the calldata selector belongs to a view (read-only) function.
    ///
    /// View functions are accessible regardless of whether the feature is activated, so that
    /// callers can still query policy state (e.g. check blocklist membership) even if the
    /// precompile feature is administratively disabled.
    fn is_view_selector(calldata: &[u8]) -> bool {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return false;
        };
        selector == IPolicyRegistry::isAuthorizedCall::SELECTOR
            || selector == IPolicyRegistry::policyExistsCall::SELECTOR
            || selector == IPolicyRegistry::policyAdminCall::SELECTOR
            || selector == IPolicyRegistry::pendingPolicyAdminCall::SELECTOR
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
    use alloy_primitives::{Address, Bytes, U256, address};
    use alloy_sol_types::{Panic, PanicKind, SolCall, SolError, SolValue};
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

    /// Activates the policy registry and writes the built-in policies to storage.
    ///
    /// Call this instead of `activate_policy_registry` when the test needs to query
    /// built-in policy IDs (`ALWAYS_ALLOW_ID`, `ALWAYS_BLOCK_ID`) directly.
    fn activate_and_init(storage: &mut HashMapStorageProvider) {
        activate_policy_registry(storage);
        StorageCtx::enter(storage, |ctx| {
            PolicyRegistryStorage::new(ctx).ensure_initialized_and_get_counter()
        })
        .unwrap();
    }

    fn deactivate_policy_registry(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .deactivate(ActivationFeature::PolicyRegistry.id(), Some(ACTIVATION_ADMIN))
                .unwrap()
        });
    }

    #[test]
    fn write_call_reverts_when_policy_registry_is_inactive() {
        let mut storage = HashMapStorageProvider::new(1);
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

        assert!(output.is_revert());
    }

    #[test]
    fn dispatch_succeeds_when_policy_registry_is_active() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_and_init(&mut storage);
        let calldata =
            IPolicyRegistry::policyExistsCall { policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID }
                .abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!output.is_revert());
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

        assert!(!output.is_revert());
        let id = IPolicyRegistry::createPolicyCall::abi_decode_returns(&output.bytes).unwrap();
        assert_eq!((id >> 56) as u8, IPolicyRegistry::PolicyType::ALLOWLIST as u8);
    }

    #[test]
    fn dispatch_create_policy_rejects_invalid_policy_type_calldata() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        storage.set_caller(ADMIN);
        let mut calldata = Vec::from(IPolicyRegistry::createPolicyCall::SELECTOR);
        calldata.extend_from_slice(&ADMIN.abi_encode());
        calldata.extend_from_slice(&[0u8; 31]);
        calldata.push(0xff);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        let expected: Bytes =
            Panic { code: U256::from(PanicKind::EnumConversionError as u32) }.abi_encode().into();
        assert!(output.is_revert());
        assert_eq!(output.bytes, expected);

        let valid_calldata = IPolicyRegistry::createPolicyCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .abi_encode();
        let valid_output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &valid_calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!valid_output.is_revert());
        let id =
            IPolicyRegistry::createPolicyCall::abi_decode_returns(&valid_output.bytes).unwrap();
        assert_eq!(id, 0x0100000000000002);
    }

    #[test]
    fn dispatch_is_authorized_always_allow_returns_true() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_and_init(&mut storage);
        let calldata = IPolicyRegistry::isAuthorizedCall {
            policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID,
            account: ALICE,
        }
        .abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should not fatally error");

        assert!(!output.is_revert());
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

        assert!(output.is_revert());
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
        assert!(!output.is_revert(), "create_allowlist_policy setup unexpectedly reverted");
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

        assert!(!output.is_revert());
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
        assert!(!out.is_revert());

        // finalize
        storage.set_caller(new_admin);
        let finalize_calldata =
            IPolicyRegistry::finalizeUpdateAdminCall { policyId: id }.abi_encode();
        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &finalize_calldata)
        })
        .unwrap();
        assert!(!out.is_revert());

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
        assert!(!out.is_revert());
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
        assert!(!out.is_revert());

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
        assert!(!blocklist_out.is_revert(), "blocklist policy creation unexpectedly reverted");
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
        assert!(!out.is_revert());
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
        assert!(!out.is_revert());
        let pending =
            IPolicyRegistry::pendingPolicyAdminCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(pending, Address::ZERO);
    }

    #[test]
    fn view_functions_succeed_when_policy_registry_deactivated() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_and_init(&mut storage);

        // Create a blocklist policy while the registry is active.
        storage.set_caller(ADMIN);
        let policy_id = {
            let calldata = IPolicyRegistry::createPolicyCall {
                admin: ADMIN,
                policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
            }
            .abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(!out.is_revert());
            IPolicyRegistry::createPolicyCall::abi_decode_returns(&out.bytes).unwrap()
        };

        // Add Alice to the blocklist.
        storage.set_caller(ADMIN);
        {
            let calldata = IPolicyRegistry::updateBlocklistCall {
                policyId: policy_id,
                blocked: true,
                accounts: alloc::vec![ALICE],
            }
            .abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(!out.is_revert());
        }

        // Deactivate the registry.
        deactivate_policy_registry(&mut storage);

        // View calls must still return current state after deactivation.
        {
            let is_authorized_calldata =
                IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account: ALICE }
                    .abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &is_authorized_calldata)
            })
            .unwrap();
            assert!(!out.is_revert(), "isAuthorized must not revert when feature is deactivated");
            let authorized =
                IPolicyRegistry::isAuthorizedCall::abi_decode_returns(&out.bytes).unwrap();
            assert!(!authorized, "Alice should remain blocked after deactivation");
        }

        {
            let calldata = IPolicyRegistry::policyExistsCall { policyId: policy_id }.abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(!out.is_revert(), "policyExists must not revert when feature is deactivated");
            let exists = IPolicyRegistry::policyExistsCall::abi_decode_returns(&out.bytes).unwrap();
            assert!(exists, "policy must still report existing after deactivation");
        }

        {
            let calldata = IPolicyRegistry::policyAdminCall { policyId: policy_id }.abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(!out.is_revert(), "policyAdmin must not revert when feature is deactivated");
            let admin = IPolicyRegistry::policyAdminCall::abi_decode_returns(&out.bytes).unwrap();
            assert_eq!(admin, ADMIN, "policy admin must remain after deactivation");
        }

        // Write calls must still revert when deactivated.
        storage.set_caller(ADMIN);
        {
            let calldata = IPolicyRegistry::createPolicyCall {
                admin: ADMIN,
                policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
            }
            .abi_encode();
            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(out.is_revert(), "createPolicy must revert when feature is deactivated");
        }
    }
}
