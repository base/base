//! ABI dispatch and state transitions for the activation registry.

use alloc::string::ToString;

use alloy_primitives::{B256, Bytes};
use alloy_sol_types::{SolCall, SolEvent as _};
use base_precompile_storage::{BasePrecompileError, Handler, Result, StorageCtx};
use revm::precompile::{PrecompileOutput, PrecompileResult};

use super::{
    ACTIVATION_ADMIN_ADDRESS, ACTIVATION_REGISTRY_ADDRESS, ActivationRegistry,
    ActivationRegistryStorage, IActivationRegistry,
};

impl ActivationRegistry {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        match self.handle_call(ctx, calldata) {
            Ok(output) => Ok(output),
            Err(error) => ctx.error_result(error),
        }
    }

    /// Returns true when the feature is activated.
    pub fn is_activated(self, ctx: StorageCtx<'_>, feature: B256) -> Result<bool> {
        ActivationRegistryStorage::new(ctx).features.at(&feature).read()
    }

    /// Reverts unless the feature is activated.
    ///
    /// Both the activated and deactivated paths return `Ok`; callers must inspect
    /// [`PrecompileOutput::reverted`] to distinguish an activated feature from an ABI revert.
    pub fn assert_activated(self, ctx: StorageCtx<'_>, feature: B256) -> PrecompileResult {
        match self.is_activated(ctx, feature) {
            Ok(true) => Ok(ctx.success_output(Bytes::new())),
            Ok(false) => Ok(ctx.abi_revert(
                IActivationRegistry::IActivationRegistryErrors::FeatureNotActivated(
                    IActivationRegistry::FeatureNotActivated { feature },
                ),
            )),
            Err(error) => ctx.error_result(error),
        }
    }

    /// Activates the feature.
    pub fn activate(self, ctx: StorageCtx<'_>, feature: B256) -> Result<PrecompileOutput> {
        self.set_activated(ctx, feature, true)
    }

    /// Deactivates the feature.
    pub fn deactive(self, ctx: StorageCtx<'_>, feature: B256) -> Result<PrecompileOutput> {
        self.set_activated(ctx, feature, false)
    }

    /// Sets the feature activation state.
    pub fn set_activated(
        self,
        ctx: StorageCtx<'_>,
        feature: B256,
        activated: bool,
    ) -> Result<PrecompileOutput> {
        // Keep this guard at the shared mutation boundary so `activate`, `deactive`, and direct
        // `set_activated` callers all get the same static-call behavior after calldata validation.
        if ctx.is_static() {
            return Ok(ctx.abi_revert(
                IActivationRegistry::IActivationRegistryErrors::StaticCallNotAllowed(
                    IActivationRegistry::StaticCallNotAllowed {},
                ),
            ));
        }

        let caller = ctx.caller();
        if caller != ACTIVATION_ADMIN_ADDRESS {
            return Ok(ctx.abi_revert(
                IActivationRegistry::IActivationRegistryErrors::Unauthorized(
                    IActivationRegistry::Unauthorized { caller },
                ),
            ));
        }

        let mut storage = ActivationRegistryStorage::new(ctx);
        let current = storage.features.at(&feature).read()?;
        if current == activated {
            let error = if activated {
                IActivationRegistry::IActivationRegistryErrors::AlreadyActivated(
                    IActivationRegistry::AlreadyActivated { feature },
                )
            } else {
                IActivationRegistry::IActivationRegistryErrors::AlreadyDeactivated(
                    IActivationRegistry::AlreadyDeactivated { feature },
                )
            };
            return Ok(ctx.abi_revert(error));
        }

        if activated {
            storage.features.at_mut(&feature).write(true)?;
            ctx.emit_event(
                ACTIVATION_REGISTRY_ADDRESS,
                IActivationRegistry::FeatureActivated { feature, caller }.encode_log_data(),
            )?;
        } else {
            storage.features.at_mut(&feature).delete()?;
            ctx.emit_event(
                ACTIVATION_REGISTRY_ADDRESS,
                IActivationRegistry::FeatureDeactivated { feature, caller }.encode_log_data(),
            )?;
        }

        Ok(ctx.success_output(Bytes::new()))
    }

    /// Returns the calldata selector, padding short calldata with zeroes.
    pub fn calldata_selector(calldata: &[u8]) -> [u8; 4] {
        let mut selector = [0u8; 4];
        let len = calldata.len().min(selector.len());
        selector[..len].copy_from_slice(&calldata[..len]);
        selector
    }

    /// Decodes an activation registry call.
    pub fn decode_call(calldata: &[u8]) -> Result<IActivationRegistry::IActivationRegistryCalls> {
        let selector = Self::calldata_selector(calldata);
        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }

        // Match selectors explicitly so unknown selectors remain distinct from known selectors
        // whose arguments fail ABI decoding.
        match selector {
            IActivationRegistry::isActivatedCall::SELECTOR => {
                IActivationRegistry::isActivatedCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::isActivated)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::activateCall::SELECTOR => {
                IActivationRegistry::activateCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::activate)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::deactiveCall::SELECTOR => {
                IActivationRegistry::deactiveCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::deactive)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::adminCall::SELECTOR => {
                IActivationRegistry::adminCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::admin)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            _ => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
        }
    }

    /// Handles the decoded activation registry call.
    pub fn handle_call(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<PrecompileOutput> {
        let call = Self::decode_call(calldata)?;

        match call {
            IActivationRegistry::IActivationRegistryCalls::isActivated(call) => {
                let activated = self.is_activated(ctx, call.feature)?;
                Ok(ctx.success_output(
                    IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into(),
                ))
            }
            IActivationRegistry::IActivationRegistryCalls::activate(call) => {
                self.activate(ctx, call.feature)
            }
            IActivationRegistry::IActivationRegistryCalls::deactive(call) => {
                self.deactive(ctx, call.feature)
            }
            IActivationRegistry::IActivationRegistryCalls::admin(_) => Ok(ctx.success_output(
                IActivationRegistry::adminCall::abi_encode_returns(&self.admin()).into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, address};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};
    use revm::precompile::PrecompileOutput;

    use super::*;
    use crate::SECURITIES_TOKEN_CREATION;

    fn execute_with(
        storage: &mut HashMapStorageProvider,
        caller: Address,
        calldata: Bytes,
    ) -> PrecompileOutput {
        storage.set_caller(caller);
        StorageCtx::enter(storage, |ctx| ActivationRegistry::new().dispatch(ctx, &calldata))
            .expect("precompile execution should not fail fatally")
    }

    fn assert_activated(storage: &mut HashMapStorageProvider, expected: bool) {
        StorageCtx::enter(storage, |ctx| {
            assert_eq!(
                ActivationRegistry::new()
                    .is_activated(ctx, SECURITIES_TOKEN_CREATION)
                    .expect("storage read succeeds"),
                expected
            );
        });
    }

    fn activate_feature(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        let calldata =
            IActivationRegistry::activateCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();
        execute_with(storage, ACTIVATION_ADMIN_ADDRESS, calldata.into())
    }

    fn deactive_feature(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        let calldata =
            IActivationRegistry::deactiveCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();
        execute_with(storage, ACTIVATION_ADMIN_ADDRESS, calldata.into())
    }

    #[test]
    fn feature_is_inactive_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        assert_activated(&mut storage, false);
    }

    #[test]
    fn admin_can_activate_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = activate_feature(&mut storage);

        assert!(!output.reverted);
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 1);
    }

    #[test]
    fn unauthorized_caller_cannot_activate_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = address!("0x0000000000000000000000000000000000000001");
        let calldata =
            IActivationRegistry::activateCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, caller, calldata.into());

        assert!(output.reverted);
        assert_activated(&mut storage, false);
    }

    #[test]
    fn activated_feature_cannot_be_activated_again() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata: Bytes =
            IActivationRegistry::activateCall { feature: SECURITIES_TOKEN_CREATION }
                .abi_encode()
                .into();

        let first = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.clone());
        let second = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata);

        assert!(!first.reverted);
        assert!(second.reverted);
        assert_activated(&mut storage, true);
    }

    #[test]
    fn admin_can_deactive_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        let first = activate_feature(&mut storage);
        let second = deactive_feature(&mut storage);

        assert!(!first.reverted);
        assert!(!second.reverted);
        assert_activated(&mut storage, false);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 2);
    }

    #[test]
    fn deactivated_feature_can_be_reactivated() {
        let mut storage = HashMapStorageProvider::new(1);

        let first = activate_feature(&mut storage);
        let second = deactive_feature(&mut storage);
        let third = activate_feature(&mut storage);

        assert!(!first.reverted);
        assert!(!second.reverted);
        assert!(!third.reverted);
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 3);
    }

    #[test]
    fn deactivated_feature_cannot_be_deactivated_again() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = deactive_feature(&mut storage);

        assert!(output.reverted);
        assert_activated(&mut storage, false);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 0);
    }

    #[test]
    fn unauthorized_caller_cannot_deactive_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = address!("0x0000000000000000000000000000000000000001");
        let calldata =
            IActivationRegistry::deactiveCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        activate_feature(&mut storage);
        let output = execute_with(&mut storage, caller, calldata.into());

        assert!(output.reverted);
        assert_activated(&mut storage, true);
    }

    #[test]
    fn static_call_cannot_activate_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_static(true);
        let calldata =
            IActivationRegistry::activateCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.into());

        assert!(output.reverted);
        assert_activated(&mut storage, false);
    }

    #[test]
    fn static_call_cannot_deactive_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_feature(&mut storage);
        storage.set_static(true);
        let calldata =
            IActivationRegistry::deactiveCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.into());

        assert!(output.reverted);
        assert_activated(&mut storage, true);
    }

    #[test]
    fn view_call_returns_activation_state() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IActivationRegistry::isActivatedCall { feature: SECURITIES_TOKEN_CREATION }
            .abi_encode();

        let output = execute_with(&mut storage, Address::ZERO, calldata.into());
        let activated = IActivationRegistry::isActivatedCall::abi_decode_returns(&output.bytes)
            .expect("return data decodes");

        assert!(!output.reverted);
        assert!(!activated);
    }

    #[test]
    fn view_call_reflects_activated_state() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IActivationRegistry::isActivatedCall { feature: SECURITIES_TOKEN_CREATION }
            .abi_encode();

        activate_feature(&mut storage);
        let activated_output = execute_with(&mut storage, Address::ZERO, calldata.clone().into());
        let activated =
            IActivationRegistry::isActivatedCall::abi_decode_returns(&activated_output.bytes)
                .expect("return data decodes");
        deactive_feature(&mut storage);
        let deactivated_output = execute_with(&mut storage, Address::ZERO, calldata.into());
        let deactivated =
            IActivationRegistry::isActivatedCall::abi_decode_returns(&deactivated_output.bytes)
                .expect("return data decodes");

        assert!(!activated_output.reverted);
        assert!(activated);
        assert!(!deactivated_output.reverted);
        assert!(!deactivated);
    }

    #[test]
    fn assert_activated_reverts_after_deactive() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage);
        let activated_output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistry::new().assert_activated(ctx, SECURITIES_TOKEN_CREATION)
        })
        .expect("feature should be activated");
        deactive_feature(&mut storage);
        let deactivated_output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistry::new().assert_activated(ctx, SECURITIES_TOKEN_CREATION)
        })
        .expect("deactivated feature should return an ABI revert");

        assert!(!activated_output.reverted);
        assert!(deactivated_output.reverted);
    }

    #[test]
    fn short_calldata_preserves_partial_selector() {
        let Err(error) = ActivationRegistry::decode_call(&[0xab, 0xcd]) else {
            panic!("selector is short");
        };

        assert_eq!(error, BasePrecompileError::UnknownFunctionSelector([0xab, 0xcd, 0, 0]));
    }

    #[test]
    fn malformed_known_selector_returns_decode_error() {
        let Err(error) =
            ActivationRegistry::decode_call(&IActivationRegistry::isActivatedCall::SELECTOR)
        else {
            panic!("arguments are missing");
        };

        assert!(matches!(
            error,
            BasePrecompileError::AbiDecodeFailed {
                selector: IActivationRegistry::isActivatedCall::SELECTOR,
                ..
            }
        ));
    }

    #[test]
    fn malformed_deactive_selector_returns_decode_error() {
        let Err(error) =
            ActivationRegistry::decode_call(&IActivationRegistry::deactiveCall::SELECTOR)
        else {
            panic!("arguments are missing");
        };

        assert!(matches!(
            error,
            BasePrecompileError::AbiDecodeFailed {
                selector: IActivationRegistry::deactiveCall::SELECTOR,
                ..
            }
        ));
    }
}
