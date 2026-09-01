//! ABI dispatch for the activation registry.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, PrecompileResult, StorageCtx};

use crate::{
    ActivationAdminConfig, ActivationRegistryStorage,
    IActivationRegistry::{self, IActivationRegistryCalls as C},
    NoopPrecompileCallObserver, PrecompileCallObserver, PrecompileCallRecorder,
    PrecompileMetricLabels,
    macros::decode_precompile_call,
};

impl ActivationRegistryStorage<'_> {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with_observer(
            ctx,
            calldata,
            admin_config,
            upgrade,
            NoopPrecompileCallObserver,
        )
    }

    /// ABI-dispatches activation registry calldata with an observer.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder = PrecompileCallRecorder::start(
            observer,
            PrecompileMetricLabels::activation_call(calldata),
        );
        // Activation-registry selectors are all nonpayable; reject attached ETH from Cobalt
        // onward. Pre-Cobalt (Beryl) preserves the historical accept-and-strand behavior so
        // replay of live-installed activation calls remains byte-identical.
        if upgrade >= BaseUpgrade::Cobalt && !ctx.call_value().is_zero() {
            return recorder.record_base_error_result(
                ctx,
                BasePrecompileError::revert(IActivationRegistry::NonPayable {}),
            );
        }
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        recorder.record_base_result(ctx, self.inner(calldata, admin_config), |output| output)
    }

    fn inner(
        &mut self,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
    ) -> base_precompile_storage::Result<Bytes> {
        let set_admin_selector = IActivationRegistry::setAdminCall::SELECTOR;
        if !admin_config.state_enabled && calldata.get(..4) == Some(set_admin_selector.as_slice()) {
            return Err(BasePrecompileError::UnknownFunctionSelector(set_admin_selector));
        }

        match decode_precompile_call!(calldata, IActivationRegistry::IActivationRegistryCalls) {
            C::isActivated(call) => {
                let activated = self.is_activated(call.feature)?;
                Ok(IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into())
            }
            C::checkActivated(call) => {
                self.ensure_activated(call.feature)?;
                Ok(Bytes::new())
            }
            C::activate(call) => {
                self.activate(call.feature, admin_config)?;
                Ok(Bytes::new())
            }
            C::deactivate(call) => {
                self.deactivate(call.feature, admin_config)?;
                Ok(Bytes::new())
            }
            C::setAdmin(call) => {
                self.set_admin(call.newAdmin, admin_config)?;
                Ok(Bytes::new())
            }
            C::admin(_) => {
                Ok(IActivationRegistry::adminCall::abi_encode_returns(&self.admin(admin_config)?)
                    .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_sol_types::{SolCall, SolError};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{ActivationAdminConfig, ActivationRegistryStorage, IActivationRegistry};

    const ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const NEW_ADMIN: Address = address!("0xcd00000000000000000000000000000000000000");
    const STATIC_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::static_fallback(Some(ADMIN));
    const STATE_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::state_backed(Some(ADMIN));

    #[test]
    fn dispatch_treats_set_admin_as_unknown_before_state_backed_admin() {
        let malformed = Bytes::copy_from_slice(&IActivationRegistry::setAdminCall::SELECTOR);
        let valid =
            Bytes::from(IActivationRegistry::setAdminCall { newAdmin: NEW_ADMIN }.abi_encode());

        for calldata in [malformed, valid] {
            let mut storage = HashMapStorageProvider::new(1);
            storage.set_caller(ADMIN);

            let output = StorageCtx::enter(&mut storage, |ctx| {
                ActivationRegistryStorage::new(ctx).dispatch(
                    ctx,
                    &calldata,
                    STATIC_ADMIN_CONFIG,
                    BaseUpgrade::Beryl,
                )
            })
            .expect("unknown selector must be returned as a revert");

            assert!(output.is_revert(), "setAdmin must revert before Cobalt");
            assert_eq!(
                output.bytes,
                Bytes::copy_from_slice(&IActivationRegistry::setAdminCall::SELECTOR),
                "pre-Cobalt setAdmin must preserve the legacy unknown-selector output"
            );
        }
    }

    #[test]
    fn dispatch_accepts_set_admin_when_state_backed_admin_is_enabled() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(ADMIN);

        let calldata =
            Bytes::from(IActivationRegistry::setAdminCall { newAdmin: NEW_ADMIN }.abi_encode());

        let output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistryStorage::new(ctx).dispatch(
                ctx,
                &calldata,
                STATE_ADMIN_CONFIG,
                BaseUpgrade::Cobalt,
            )
        })
        .expect("setAdmin must not fatally error");

        assert!(output.is_success(), "setAdmin must succeed once state-backed admin is enabled");
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value_at_cobalt() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(ADMIN);
        storage.set_call_value(U256::from(1u64));

        // A read-only, unauthenticated selector — the exact path an attacker uses to strand ETH.
        let calldata = IActivationRegistry::isActivatedCall { feature: B256::ZERO }.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistryStorage::new(ctx).dispatch(
                ctx,
                &calldata,
                STATE_ADMIN_CONFIG,
                BaseUpgrade::Cobalt,
            )
        })
        .expect("nonzero value must revert, not fatally error");

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(IActivationRegistry::NonPayable {}.abi_encode()));
    }

    #[test]
    fn dispatch_accepts_nonzero_value_before_cobalt() {
        // Consensus safety: pre-Cobalt (Beryl) must NOT emit the new NonPayable revert, otherwise
        // replay of live-installed activation calls that carried ETH would diverge.
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(ADMIN);
        storage.set_call_value(U256::from(1u64));

        let calldata = IActivationRegistry::isActivatedCall { feature: B256::ZERO }.abi_encode();

        let output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistryStorage::new(ctx).dispatch(
                ctx,
                &calldata,
                STATIC_ADMIN_CONFIG,
                BaseUpgrade::Beryl,
            )
        })
        .expect("Beryl replay must not fatally error on nonzero value");

        assert!(output.is_success(), "pre-Cobalt must accept nonzero value (legacy behavior)");
        assert_ne!(
            output.bytes,
            Bytes::from(IActivationRegistry::NonPayable {}.abi_encode()),
            "pre-Cobalt must never emit NonPayable",
        );
    }
}
