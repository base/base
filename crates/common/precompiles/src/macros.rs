//! Runtime helpers for wrapping native precompile dispatch.

macro_rules! base_precompile {
    ($id:expr, |$ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(::revm::precompile::PrecompileOutput::new_reverted(
                        0,
                        ::alloy_primitives::Bytes::new(),
                    ));
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
                    return Ok(::revm::precompile::PrecompileOutput::new_reverted(
                        0,
                        ::alloy_primitives::Bytes::new(),
                    ));
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

macro_rules! deduct_calldata_cost {
    ($ctx:expr, $calldata:expr $(,)?) => {{
        const G_SHA3WORD: u64 = 6;

        let calldata_len = $calldata.len();
        let calldata_cost = calldata_len.div_ceil(32).saturating_mul(G_SHA3WORD as usize) as u64;
        if let Err(e) = $ctx.deduct_gas(calldata_cost) {
            return e.into_precompile_result($ctx.gas_used());
        }
    }};
}

pub(crate) use deduct_calldata_cost;

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

        <$call_ty as ::alloy_sol_types::SolInterface>::abi_decode(calldata).map_err(|_| {
            ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector)
        })?
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
    fn decode_precompile_call_decodes_known_call() {
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();
        let call = decode_policy_call(&calldata).unwrap();

        assert!(matches!(call, IPolicyRegistry::IPolicyRegistryCalls::policyExists(_)));
    }
}
