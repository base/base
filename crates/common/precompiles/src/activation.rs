use alloc::string::ToString;

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, B256, Bytes, address, b256};
use alloy_sol_types::{SolCall, SolError as _, SolEvent as _, sol};
use base_precompile_macros::contract;
use base_precompile_storage::{
    BasePrecompileError, EvmPrecompileStorageProvider, Handler, Mapping, Result, StorageCtx,
};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

/// Activation registry precompile address.
pub const ACTIVATION_REGISTRY_ADDRESS: Address =
    address!("0x84530000000000000000000000000000000000ff");

/// Temporary activation admin address.
///
/// Replace this with the final Base-controlled activation signer before deployment.
pub const ACTIVATION_ADMIN_ADDRESS: Address =
    address!("0xcb00000000000000000000000000000000000000");

/// Security-token factory creation feature id.
pub const SECURITIES_TOKEN_CREATION: B256 =
    b256!("0x89e4523f0886ce01d76094212ed707081da92a45221e22c15c5689be470db63e");

sol! {
    /// Activation registry ABI.
    interface IActivationRegistry {
        /// Emitted when a feature is enabled.
        event FeatureEnabled(bytes32 indexed feature, address indexed caller);

        /// Caller is not authorized to enable features.
        error Unauthorized(address caller);

        /// Feature is already enabled.
        error AlreadyEnabled(bytes32 feature);

        /// Feature is not enabled.
        error FeatureNotEnabled(bytes32 feature);

        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// State-mutating call was attempted in a static context.
        error StaticCallNotAllowed();

        /// Returns true when `feature` is enabled.
        function isEnabled(bytes32 feature) external view returns (bool);

        /// Enables `feature`.
        function enable(bytes32 feature) external;

        /// Returns the activation admin.
        function activationAdmin() external view returns (address);
    }
}

/// Storage layout for the activation registry.
#[contract(addr = ACTIVATION_REGISTRY_ADDRESS)]
pub struct ActivationRegistryStorage {
    /// Runtime activation flags keyed by feature id.
    pub features: Mapping<B256, bool>,
}

/// Runtime activation registry for Base-native features.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Creates a new activation registry handle.
    pub const fn new() -> Self {
        Self
    }

    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn create_precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("ActivationRegistry".into()), Self::run)
    }

    /// Executes the activation registry precompile.
    pub fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            // No gas charged: the call type is invalid before any work is performed.
            return Ok(PrecompileOutput::new_reverted(
                0,
                IActivationRegistry::DelegateCallNotAllowed {}.abi_encode().into(),
            ));
        }

        let data = input.data;
        let mut storage = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut storage, |ctx| Self::new().dispatch(ctx, data))
    }

    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        match self.inner(ctx, calldata) {
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
        if storage.features.at(&feature).read()? {
            return Ok(ctx.abi_revert(
                IActivationRegistry::IActivationRegistryErrors::AlreadyEnabled(
                    IActivationRegistry::AlreadyEnabled { feature },
                ),
            ));
        }

        storage.features.at_mut(&feature).write(true)?;
        ctx.emit_event(
            ACTIVATION_REGISTRY_ADDRESS,
            IActivationRegistry::FeatureEnabled { feature, caller }.encode_log_data(),
        )?;

        Ok(ctx.success_output(Bytes::new()))
    }

    /// Returns the activation admin.
    pub const fn activation_admin(self) -> Address {
        ACTIVATION_ADMIN_ADDRESS
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

    /// Runs the decoded activation registry call.
    pub fn inner(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<PrecompileOutput> {
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
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;

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

    #[test]
    fn feature_is_disabled_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        assert_enabled(&mut storage, false);
    }

    #[test]
    fn activation_admin_can_enable_feature() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata =
            IActivationRegistry::enableCall { feature: SECURITIES_TOKEN_CREATION }.abi_encode();

        let output = execute_with(&mut storage, ACTIVATION_ADMIN_ADDRESS, calldata.into());

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
}
