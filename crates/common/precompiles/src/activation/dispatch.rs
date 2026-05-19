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

    /// Returns true when the feature is enabled.
    pub fn is_enabled(self, ctx: StorageCtx<'_>, feature: B256) -> Result<bool> {
        ActivationRegistryStorage::new(ctx).features.at(&feature).read()
    }

    /// Reverts unless the feature is enabled.
    ///
    /// Both the enabled and disabled paths return `Ok`; callers must inspect
    /// [`PrecompileOutput::reverted`] to distinguish an enabled feature from an ABI revert.
    pub fn assert_enabled(self, ctx: StorageCtx<'_>, feature: B256) -> PrecompileResult {
        match self.is_enabled(ctx, feature) {
            Ok(true) => Ok(ctx.success_output(Bytes::new())),
            Ok(false) => Ok(ctx.abi_revert(
                IActivationRegistry::IActivationRegistryErrors::FeatureNotEnabled(
                    IActivationRegistry::FeatureNotEnabled { feature },
                ),
            )),
            Err(error) => ctx.error_result(error),
        }
    }

    /// Enables the feature.
    pub fn enable(self, ctx: StorageCtx<'_>, feature: B256) -> Result<PrecompileOutput> {
        self.set_enabled(ctx, feature, true)
    }

    /// Disables the feature.
    pub fn disable(self, ctx: StorageCtx<'_>, feature: B256) -> Result<PrecompileOutput> {
        self.set_enabled(ctx, feature, false)
    }

    /// Sets the feature state.
    pub fn set_enabled(
        self,
        ctx: StorageCtx<'_>,
        feature: B256,
        enabled: bool,
    ) -> Result<PrecompileOutput> {
        // Keep this guard at the shared mutation boundary so `enable`, `disable`, and direct
        // `set_enabled` callers all get the same static-call behavior after calldata validation.
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
        if current == enabled {
            let error = if enabled {
                IActivationRegistry::IActivationRegistryErrors::AlreadyEnabled(
                    IActivationRegistry::AlreadyEnabled { feature },
                )
            } else {
                IActivationRegistry::IActivationRegistryErrors::AlreadyDisabled(
                    IActivationRegistry::AlreadyDisabled { feature },
                )
            };
            return Ok(ctx.abi_revert(error));
        }

        if enabled {
            storage.features.at_mut(&feature).write(true)?;
            ctx.emit_event(
                ACTIVATION_REGISTRY_ADDRESS,
                IActivationRegistry::FeatureEnabled { feature, caller }.encode_log_data(),
            )?;
        } else {
            storage.features.at_mut(&feature).delete()?;
            ctx.emit_event(
                ACTIVATION_REGISTRY_ADDRESS,
                IActivationRegistry::FeatureDisabled { feature, caller }.encode_log_data(),
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
            IActivationRegistry::isEnabledCall::SELECTOR => {
                IActivationRegistry::isEnabledCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::isEnabled)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::enableCall::SELECTOR => {
                IActivationRegistry::enableCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::enable)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::disableCall::SELECTOR => {
                IActivationRegistry::disableCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::disable)
                    .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: error.to_string(),
                    })
            }
            IActivationRegistry::activationAdminCall::SELECTOR => {
                IActivationRegistry::activationAdminCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::activationAdmin)
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
            IActivationRegistry::IActivationRegistryCalls::isEnabled(call) => {
                let enabled = self.is_enabled(ctx, call.feature)?;
                Ok(ctx.success_output(
                    IActivationRegistry::isEnabledCall::abi_encode_returns(&enabled).into(),
                ))
            }
            IActivationRegistry::IActivationRegistryCalls::enable(call) => {
                self.enable(ctx, call.feature)
            }
            IActivationRegistry::IActivationRegistryCalls::disable(call) => {
                self.disable(ctx, call.feature)
            }
            IActivationRegistry::IActivationRegistryCalls::activationAdmin(_) => Ok(ctx
                .success_output(
                    IActivationRegistry::activationAdminCall::abi_encode_returns(
                        &self.activation_admin(),
                    )
                    .into(),
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

    fn assert_enabled(storage: &mut HashMapStorageProvider, expected: bool) {
        StorageCtx::enter(storage, |ctx| {
            assert_eq!(
                ActivationRegistry::new()
                    .is_enabled(ctx, SECURITIES_TOKEN_CREATION)
                    .expect("storage read succeeds"),
                expected
            );
        });
    }

    fn enable_feature(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        let calldata =
            IActivationRegistry::enableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();
        execute_with(storage, ACTIVATION_ADMIN_ADDRESS, calldata.into())
    }

    fn disable_feature(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        let calldata =
            IActivationRegistry::disableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();
        execute_with(storage, ACTIVATION_ADMIN_ADDRESS, calldata.into())
    }

    #[test]
    fn feature_is_disabled_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        assert_enabled(&mut storage, false);
    }

    #[test]
    fn activation_admin_can_enable_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = enable_feature(&mut storage);

        assert!(!output.reverted);
        assert_enabled(&mut storage, true);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 1);
    }

    #[test]
    fn unauthorized_caller_cannot_enable_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = address!("0x0000000000000000000000000000000000000001");
        let calldata =
            IActivationRegistry::enableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, caller, calldata.into());

        assert!(output.reverted);
        assert_enabled(&mut storage, false);
    }

    #[test]
    fn enabled_feature_cannot_be_enabled_again() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata: Bytes =
            IActivationRegistry::enableCall { feature: SECURITIES_TOKEN_CREATION }
                .abi_encode()
                .into();

        let first = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.clone());
        let second = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata);

        assert!(!first.reverted);
        assert!(second.reverted);
        assert_enabled(&mut storage, true);
    }

    #[test]
    fn activation_admin_can_disable_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        let first = enable_feature(&mut storage);
        let second = disable_feature(&mut storage);

        assert!(!first.reverted);
        assert!(!second.reverted);
        assert_enabled(&mut storage, false);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 2);
    }

    #[test]
    fn disabled_feature_can_be_reenabled() {
        let mut storage = HashMapStorageProvider::new(1);

        let first = enable_feature(&mut storage);
        let second = disable_feature(&mut storage);
        let third = enable_feature(&mut storage);

        assert!(!first.reverted);
        assert!(!second.reverted);
        assert!(!third.reverted);
        assert_enabled(&mut storage, true);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 3);
    }

    #[test]
    fn disabled_feature_cannot_be_disabled_again() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = disable_feature(&mut storage);

        assert!(output.reverted);
        assert_enabled(&mut storage, false);
        assert_eq!(storage.get_events(ACTIVATION_REGISTRY_ADDRESS).len(), 0);
    }

    #[test]
    fn unauthorized_caller_cannot_disable_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = address!("0x0000000000000000000000000000000000000001");
        let calldata =
            IActivationRegistry::disableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        enable_feature(&mut storage);
        let output = execute_with(&mut storage, caller, calldata.into());

        assert!(output.reverted);
        assert_enabled(&mut storage, true);
    }

    #[test]
    fn static_call_cannot_enable_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_static(true);
        let calldata =
            IActivationRegistry::enableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.into());

        assert!(output.reverted);
        assert_enabled(&mut storage, false);
    }

    #[test]
    fn static_call_cannot_disable_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        enable_feature(&mut storage);
        storage.set_static(true);
        let calldata =
            IActivationRegistry::disableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.into());

        assert!(output.reverted);
        assert_enabled(&mut storage, true);
    }

    #[test]
    fn view_call_returns_activation_state() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata =
            IActivationRegistry::isEnabledCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, Address::ZERO, calldata.into());
        let enabled = IActivationRegistry::isEnabledCall::abi_decode_returns(&output.bytes)
            .expect("return data decodes");

        assert!(!output.reverted);
        assert!(!enabled);
    }

    #[test]
    fn view_call_reflects_disabled_state_after_enable() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata =
            IActivationRegistry::isEnabledCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        enable_feature(&mut storage);
        let enabled_output = execute_with(&mut storage, Address::ZERO, calldata.clone().into());
        let enabled = IActivationRegistry::isEnabledCall::abi_decode_returns(&enabled_output.bytes)
            .expect("return data decodes");
        disable_feature(&mut storage);
        let disabled_output = execute_with(&mut storage, Address::ZERO, calldata.into());
        let disabled =
            IActivationRegistry::isEnabledCall::abi_decode_returns(&disabled_output.bytes)
                .expect("return data decodes");

        assert!(!enabled_output.reverted);
        assert!(enabled);
        assert!(!disabled_output.reverted);
        assert!(!disabled);
    }

    #[test]
    fn assert_enabled_reverts_after_disable() {
        let mut storage = HashMapStorageProvider::new(1);

        enable_feature(&mut storage);
        let enabled_output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistry::new().assert_enabled(ctx, SECURITIES_TOKEN_CREATION)
        })
        .expect("feature should be enabled");
        disable_feature(&mut storage);
        let disabled_output = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistry::new().assert_enabled(ctx, SECURITIES_TOKEN_CREATION)
        })
        .expect("disabled feature should return an ABI revert");

        assert!(!enabled_output.reverted);
        assert!(disabled_output.reverted);
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
            ActivationRegistry::decode_call(&IActivationRegistry::isEnabledCall::SELECTOR)
        else {
            panic!("arguments are missing");
        };

        assert!(matches!(
            error,
            BasePrecompileError::AbiDecodeFailed {
                selector: IActivationRegistry::isEnabledCall::SELECTOR,
                ..
            }
        ));
    }

    #[test]
    fn malformed_disable_selector_returns_decode_error() {
        let Err(error) =
            ActivationRegistry::decode_call(&IActivationRegistry::disableCall::SELECTOR)
        else {
            panic!("arguments are missing");
        };

        assert!(matches!(
            error,
            BasePrecompileError::AbiDecodeFailed {
                selector: IActivationRegistry::disableCall::SELECTOR,
                ..
            }
        ));
    }
}
