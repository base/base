//! ABI dispatch for the activation registry.

use alloc::string::ToString;

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, Result, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{ActivationRegistry, IActivationRegistry};

impl ActivationRegistry {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.handle_call(ctx, calldata).into_precompile_result(ctx.gas_used(), |output| output)
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
            IActivationRegistry::deactivateCall::SELECTOR => {
                IActivationRegistry::deactivateCall::abi_decode(calldata)
                    .map(IActivationRegistry::IActivationRegistryCalls::deactivate)
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
    pub fn handle_call(self, ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<Bytes> {
        let call = Self::decode_call(calldata)?;

        match call {
            IActivationRegistry::IActivationRegistryCalls::isActivated(call) => {
                let activated = self.is_activated(ctx, call.feature)?;
                Ok(IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into())
            }
            IActivationRegistry::IActivationRegistryCalls::activate(call) => {
                self.activate(ctx, call.feature)?;
                Ok(Bytes::new())
            }
            IActivationRegistry::IActivationRegistryCalls::deactivate(call) => {
                self.deactivate(ctx, call.feature)?;
                Ok(Bytes::new())
            }
            IActivationRegistry::IActivationRegistryCalls::admin(_) => {
                Ok(IActivationRegistry::adminCall::abi_encode_returns(&self.admin()).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};
    use revm::precompile::PrecompileOutput;
    use rstest::rstest;

    use super::*;

    const FEATURE: B256 = ActivationRegistry::SECURITIES_TOKEN_CREATION;

    fn execute_with(
        storage: &mut HashMapStorageProvider,
        caller: Address,
        calldata: Bytes,
    ) -> PrecompileOutput {
        storage.set_caller(caller);
        StorageCtx::enter(storage, |ctx| ActivationRegistry::new().dispatch(ctx, &calldata))
            .expect("precompile execution should not fail fatally")
    }

    fn activate_feature(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        let calldata = IActivationRegistry::activateCall { feature: FEATURE }.abi_encode();
        execute_with(storage, ActivationRegistry::ADMIN, calldata.into())
    }

    #[rstest]
    #[case::inactive(false)]
    #[case::active(true)]
    fn view_call_returns_activation_state(#[case] initially_active: bool) {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IActivationRegistry::isActivatedCall { feature: FEATURE }.abi_encode();

        if initially_active {
            activate_feature(&mut storage);
        }
        let output = execute_with(&mut storage, Address::ZERO, calldata.into());
        let activated = IActivationRegistry::isActivatedCall::abi_decode_returns(&output.bytes)
            .expect("return data decodes");

        assert!(!output.reverted);
        assert_eq!(activated, initially_active);
    }

    #[test]
    fn short_calldata_preserves_partial_selector() {
        let Err(error) = ActivationRegistry::decode_call(&[0xab, 0xcd]) else {
            panic!("selector is short");
        };

        assert_eq!(error, BasePrecompileError::UnknownFunctionSelector([0xab, 0xcd, 0, 0]));
    }

    #[rstest]
    #[case::is_activated(IActivationRegistry::isActivatedCall::SELECTOR)]
    #[case::deactivate(IActivationRegistry::deactivateCall::SELECTOR)]
    fn malformed_known_selector_returns_decode_error(#[case] selector: [u8; 4]) {
        let Err(error) = ActivationRegistry::decode_call(&selector) else {
            panic!("arguments are missing");
        };

        assert!(matches!(
            error,
            BasePrecompileError::AbiDecodeFailed {
                selector: decoded_selector,
                ..
            } if decoded_selector == selector
        ));
    }
}
