//! Runtime helpers for wrapping native precompile dispatch.

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
                    ::base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
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
                return Err(
                    ::base_tiger_precompile_storage::BasePrecompileError::AbiDecodeFailed {
                        selector,
                        error: ::alloc::string::ToString::to_string(&error),
                    },
                );
            }
            Err(_) => {
                return Err(
                    ::base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
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
            ::base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector([0u8; 4]),
        )
    };
}

pub(crate) use reject_frozen_selector;

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;
    use base_tiger_precompile_storage::{BasePrecompileError, Result};

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
