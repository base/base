use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
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
            .into_precompile_result(ctx.gas_used(), ctx.state_gas_used(), |b| b)
    }

    /// Enforces the account batch size cap before any ABI decode heap allocation.
    ///
    /// `createPolicyWithAccounts`, `updateAllowlist`, and `updateBlocklist` all share the
    /// same calldata layout for their `address[]` parameter:
    ///
    /// ```text
    /// [0..4]   4-byte selector
    /// [4..36]  param0 (32 bytes)
    /// [36..68] param1 (32 bytes)
    /// [68..100] dynamic offset for address[] (32 bytes)
    /// [100..132] array length (32 bytes, big-endian)
    /// ```
    ///
    /// This method reads the raw 32-byte length word at offset 100 and returns
    /// `BatchSizeTooLarge` immediately if it exceeds `MAX_ACCOUNTS_PER_BATCH`, before
    /// the full ABI decode allocates memory for the entire array.
    fn check_batch_calldata_length(calldata: &[u8]) -> base_precompile_storage::Result<()> {
        const ARRAY_LEN_OFFSET: usize = 100;
        const ARRAY_LEN_END: usize = 132;

        let selector: [u8; 4] = match calldata.get(..4) {
            Some(s) => s.try_into().unwrap(),
            None => return Ok(()),
        };

        let is_batch_selector = selector == IPolicyRegistry::createPolicyWithAccountsCall::SELECTOR
            || selector == IPolicyRegistry::updateAllowlistCall::SELECTOR
            || selector == IPolicyRegistry::updateBlocklistCall::SELECTOR;

        if !is_batch_selector {
            return Ok(());
        }

        let Some(len_word) = calldata.get(ARRAY_LEN_OFFSET..ARRAY_LEN_END) else {
            return Ok(());
        };

        if U256::from_be_slice(len_word) > U256::from(Self::MAX_ACCOUNTS_PER_BATCH) {
            return Err(BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(Self::MAX_ACCOUNTS_PER_BATCH),
            }));
        }

        Ok(())
    }

    fn inner(&mut self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        Self::check_batch_calldata_length(calldata)?;
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

    /// Activates the policy registry and writes the built-in policies to storage.
    ///
    /// Call this instead of `activate_policy_registry` when the test needs to query
    /// built-in policy IDs (`ALWAYS_ALLOW_ID`, `ALWAYS_BLOCK_ID`) directly.
    fn activate_and_init(storage: &mut HashMapStorageProvider) {
        activate_policy_registry(storage);
        StorageCtx::enter(storage, |ctx| PolicyRegistryStorage::new(ctx).write_builtins()).unwrap();
    }

    #[test]
    fn dispatch_reverts_when_policy_registry_is_inactive() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();

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

    /// Verifies that `check_batch_calldata_length` rejects an oversized array length word
    /// before the full ABI decode allocates any heap memory for the accounts slice.
    ///
    /// The calldata is crafted to be exactly 132 bytes (selector + 3 head words + length word)
    /// with the length word set to `MAX_ACCOUNTS_PER_BATCH + 1` but no actual account data
    /// following it, so a successful decode would be impossible anyway. The check must fire
    /// first to confirm the early-rejection path.
    #[test]
    fn dispatch_batch_selectors_reject_oversized_length_before_decode() {
        let oversized_len = PolicyRegistryStorage::MAX_ACCOUNTS_PER_BATCH as u8 + 1;

        let batch_selectors = [
            IPolicyRegistry::createPolicyWithAccountsCall::SELECTOR,
            IPolicyRegistry::updateAllowlistCall::SELECTOR,
            IPolicyRegistry::updateBlocklistCall::SELECTOR,
        ];

        for selector in batch_selectors {
            let mut storage = HashMapStorageProvider::new(1);
            activate_policy_registry(&mut storage);

            // Layout: selector(4) | param0(32) | param1(32) | dynamic-offset=0x60(32) | len(32)
            let mut calldata = alloc::vec![0u8; 132];
            calldata[..4].copy_from_slice(&selector);
            // bytes [68..100]: dynamic offset = 96 (0x60)
            calldata[99] = 0x60;
            // bytes [100..132]: array length = MAX_ACCOUNTS_PER_BATCH + 1
            calldata[131] = oversized_len;

            let out = StorageCtx::enter(&mut storage, |ctx| {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
            })
            .unwrap();
            assert!(out.is_revert(), "expected revert for oversized batch selector {selector:?}");
        }
    }

    #[test]
    fn dispatch_batch_selectors_accept_max_batch_size() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let id = create_allowlist_policy(&mut storage);

        storage.set_caller(ADMIN);
        let accounts: alloc::vec::Vec<Address> = (0..PolicyRegistryStorage::MAX_ACCOUNTS_PER_BATCH)
            .map(|i| {
                Address::from_word(alloy_primitives::B256::from(alloy_primitives::U256::from(
                    i as u64 + 1,
                )))
            })
            .collect();

        let calldata =
            IPolicyRegistry::updateAllowlistCall { policyId: id, allowed: true, accounts }
                .abi_encode();

        let out = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .unwrap();
        assert!(!out.is_revert(), "max batch size should succeed");
    }
}
