//! Runtime helpers for wrapping native precompile dispatch.

/// Wraps a stateful native precompile body in the Base storage-provider setup.
macro_rules! base_precompile {
    ($id:expr, |$ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |input| {
                if !input.is_direct_call() {
                    return ::base_precompile_storage::BasePrecompileError::revert(
                        ::base_precompile_storage::DelegateCallNotAllowed {},
                    )
                    .into_precompile_result(0, 0);
                }

                let $calldata: ::alloy_primitives::Bytes = input.data.to_vec().into();
                let mut provider = ::base_precompile_storage::EvmPrecompileStorageProvider::new(
                    input,
                    ::revm::context_interface::cfg::GasParams::default(),
                );

                ::base_precompile_storage::StorageCtx::enter(&mut provider, |$ctx| $impl)
            },
        )
    }};
    ($id:expr, |$input:ident, $ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |$input| {
                if !$input.is_direct_call() {
                    return ::base_precompile_storage::BasePrecompileError::revert(
                        ::base_precompile_storage::DelegateCallNotAllowed {},
                    )
                    .into_precompile_result(0, 0);
                }

                let $calldata: ::alloy_primitives::Bytes = $input.data.to_vec().into();
                let mut provider =
                    ::base_precompile_storage::EvmPrecompileStorageProvider::new($input);

                ::base_precompile_storage::StorageCtx::enter(&mut provider, |$ctx| $impl)
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

        match <$call_ty as ::alloy_sol_types::SolInterface>::abi_decode(calldata) {
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

#[cfg(test)]
mod tests {
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
}
