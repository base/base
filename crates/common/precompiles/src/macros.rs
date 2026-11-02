//! Runtime helpers for wrapping native precompile dispatch.

/// Wraps a stateful native precompile body in the Base storage-provider setup.
///
/// `upgrade:` selects the same Ethereum gas table as the enclosing Base fork.
/// `storage_features:` is required — every caller must state the fork feature set
/// the wrapper runs under, so a future feature-sensitive field cannot silently
/// inherit `StorageFeatures::Legacy`. See [`crate::UpgradeGatedStorageFeatures::from_upgrade`].
macro_rules! base_precompile {
    ($id:expr, upgrade: $upgrade:expr, storage_features: $storage_features:expr, |$ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |input| {
                if !input.is_direct_call() {
                    return ::base_precompile_storage::IntoEnginePrecompileResult::into_revm(
                        ::base_precompile_storage::BasePrecompileError::revert(
                            ::base_precompile_storage::DelegateCallNotAllowed {},
                        )
                        .into_precompile_result(0, input.reservoir),
                    );
                }

                let $calldata: &[u8] = input.data;
                let mut provider = ::base_precompile_storage::EvmPrecompileStorageProvider::new_with_storage_features(
                    input,
                    ::revm::context_interface::cfg::GasParams::new_spec(
                        ::base_common_chains::BaseUpgradeExt::into_eth_spec($upgrade),
                    ),
                    $storage_features,
                );

                ::base_precompile_storage::IntoEnginePrecompileResult::into_revm(
                    ::base_precompile_storage::StorageCtx::enter(&mut provider, |$ctx| $impl),
                )
            },
        )
    }};
}

pub(crate) use base_precompile;

/// Decodes calldata into the requested ABI interface call or returns an unknown selector error.
macro_rules! decode_precompile_call {
    ($calldata:expr, $call_ty:ty $(,)?) => {{
        let calldata = $calldata;
        let selector = match calldata.get(..4) {
            Some(bytes) => {
                let mut selector = [0u8; 4];
                selector.copy_from_slice(bytes);
                selector
            }
            None => {
                return Err(
                    ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
                        [0u8; 4],
                    ),
                );
            }
        };

        match <$call_ty as ::alloy_sol_types::SolInterface>::abi_decode_validate(calldata) {
            Ok(call) => call,
            Err(error)
                if <$call_ty as ::alloy_sol_types::SolInterface>::valid_selector(selector) =>
            {
                return Err(::base_precompile_storage::BasePrecompileError::AbiDecodeFailed {
                    selector,
                    error: ::alloc::string::ToString::to_string(&error),
                });
            }
            Err(_) => {
                return Err(
                    ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
                        selector,
                    ),
                );
            }
        }
    }};
}

pub(crate) use decode_precompile_call;

/// Rejects a call as an unknown selector, freezing the observable behavior of every version that
/// predates the selector.
macro_rules! reject_frozen_selector {
    () => {
        ::core::result::Result::Err(
            ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector([0u8; 4]),
        )
    };
}

pub(crate) use reject_frozen_selector;

#[cfg(test)]
mod tests {
    #[test]
    fn native_amsterdam_delegate_rejection_returns_reservoir() {
        use alloy_evm::{
            EvmInternals,
            eth::EthEvmContext,
            precompiles::{Precompile, PrecompileInput},
        };
        use alloy_primitives::{Address, U256};
        use base_common_genesis::BaseUpgrade;
        use base_precompile_storage::StorageFeatures;
        use revm::{database::EmptyDB, primitives::hardfork::SpecId};
        let precompile = base_precompile!(
            "delegate-rejection-test",
            upgrade: BaseUpgrade::Denim,
            storage_features: StorageFeatures::Cobalt,
            |ctx, calldata| { let _ = calldata; Ok(ctx.success_output(alloy_primitives::Bytes::new())) },
        );
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let output = precompile
            .call(PrecompileInput {
                data: &[],
                gas: 100_000,
                reservoir: 123_456,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: Address::ZERO,
                is_static: false,
                bytecode_address: Address::from([1; 20]),
                internals: EvmInternals::from_context(&mut ctx),
            })
            .unwrap();
        assert!(
            matches!(output.status, revm::precompile::PrecompileStatus::Revert),
            "delegate calls must still revert"
        );
        assert_eq!(
            output.reservoir, 123_456,
            "early rejection must return the untouched reservoir"
        );
        assert_eq!(
            output.gas_used, 0,
            "early rejection must preserve its existing zero regular charge"
        );
        assert_eq!(output.state_gas_used, 0, "early rejection creates no state");
        assert_eq!(output.state_gas_spilled, 0, "early rejection spills no state gas");
    }

    use alloy_sol_types::SolCall;
    use base_precompile_storage::{BasePrecompileError, Result};

    use crate::IPolicyRegistry;

    fn decode_policy_call(calldata: &[u8]) -> Result<IPolicyRegistry::IPolicyRegistryCalls> {
        Ok(decode_precompile_call!(calldata, IPolicyRegistry::IPolicyRegistryCalls,))
    }

    #[test]
    fn decode_precompile_call_rejects_short_calldata() {
        let err = decode_policy_call(&[1, 2, 3]).unwrap_err();

        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    }

    #[test]
    fn decode_precompile_call_preserves_unknown_selector() {
        let err = decode_policy_call(&[1, 2, 3, 4]).unwrap_err();

        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector([1, 2, 3, 4]));
    }

    #[test]
    fn decode_precompile_call_classifies_known_selector_decode_failure() {
        let err = decode_policy_call(&IPolicyRegistry::createPolicyCall::SELECTOR).unwrap_err();

        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector: IPolicyRegistry::createPolicyCall::SELECTOR,
                ..
            }
        ));
    }

    #[test]
    fn decode_precompile_call_decodes_known_call() {
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();
        let call = decode_policy_call(&calldata).unwrap();

        assert!(matches!(call, IPolicyRegistry::IPolicyRegistryCalls::policyExists(_)));
    }

    #[test]
    fn decode_precompile_call_rejects_dirty_padding_bytes() {
        // policyExists(uint64 policyId) encodes policyId as a right-aligned 32-byte word.
        // Injecting 0xFF into the high-padding byte triggers abi_decode_validate's canonical
        // check, confirming the macro uses the validating decoder.
        let mut calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();
        calldata[4] = 0xFF;
        let err = decode_policy_call(&calldata).unwrap_err();

        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector: IPolicyRegistry::policyExistsCall::SELECTOR,
                ..
            }
        ));
    }
}
