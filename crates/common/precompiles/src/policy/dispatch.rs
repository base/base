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
    /// ABI-dispatches `calldata` to the appropriate `IPolicyRegistry` handler.
    pub(super) fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        if let Err(e) = ctx.deduct_gas(crate::input_cost(calldata.len())) {
            return e.into_precompile_result(ctx.gas_used());
        }
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationRegistryStorage::POLICY_REGISTRY)
            .and_then(|()| self.inner(calldata))
            .into_precompile_result(ctx.gas_used(), |b| b)
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, IPolicyRegistry::IPolicyRegistryCalls) {
            C::helloWorld(_) => {
                Ok(IPolicyRegistry::helloWorldCall::abi_encode_returns(&true).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{ActivationRegistryStorage, IPolicyRegistry};

    fn activate_policy_registry(storage: &mut HashMapStorageProvider) {
        const ADMIN: alloy_primitives::Address =
            alloy_primitives::address!("0xcb00000000000000000000000000000000000000");

        storage.set_caller(ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationRegistryStorage::POLICY_REGISTRY, Some(ADMIN))
                .unwrap()
        });
    }

    #[test]
    fn dispatch_reverts_when_policy_registry_is_inactive() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IPolicyRegistry::helloWorldCall {}.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should return a revert output");

        assert!(output.reverted);
    }

    #[test]
    fn dispatch_succeeds_when_policy_registry_is_active() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_policy_registry(&mut storage);
        let calldata = IPolicyRegistry::helloWorldCall {}.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch should succeed");

        assert!(!output.reverted);
        assert!(IPolicyRegistry::helloWorldCall::abi_decode_returns(&output.bytes).unwrap());
    }
}
