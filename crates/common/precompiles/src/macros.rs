//! Runtime helpers for wrapping native precompile dispatch.

/// Wraps a stateful native precompile body in the Base storage-provider setup.
///
/// For zero-value calls, the wrapper rejects insufficient calldata gas before
/// it constructs an EVM storage provider or dispatches the borrowed calldata.
/// Nonzero-value calls reach dispatch so precompile-specific nonpayable rules
/// retain their existing precedence.
///
/// `storage_features:` is required — every caller must state the fork feature set
/// the wrapper runs under, so a future feature-sensitive field cannot silently
/// inherit `StorageFeatures::Legacy`. See [`crate::UpgradeGatedStorageFeatures::from_upgrade`].
macro_rules! base_precompile {
    ($id:expr, storage_features: $storage_features:expr, |$ctx:ident, $calldata:ident| $impl:expr $(,)?) => {{
        ::alloy_evm::precompiles::DynPrecompile::new_stateful(
            ::revm::precompile::PrecompileId::Custom($id.into()),
            move |input| {
                if !input.is_direct_call() {
                    return ::base_precompile_storage::IntoEnginePrecompileResult::into_revm(
                        ::base_precompile_storage::BasePrecompileError::revert(
                            ::base_precompile_storage::DelegateCallNotAllowed {},
                        )
                        .into_precompile_result(0, 0),
                    );
                }

                let $calldata = input.data;
                let calldata_gas = $crate::PrecompileCallRecorder::<
                    $crate::NoopPrecompileCallObserver,
                >::calldata_gas_cost($calldata);
                if input.value.is_zero() && input.gas < calldata_gas {
                    return ::base_precompile_storage::IntoEnginePrecompileResult::into_revm(
                        ::base_precompile_storage::BasePrecompileError::OutOfGas
                            .into_precompile_result(0, 0),
                    );
                }
                let mut provider = ::base_precompile_storage::EvmPrecompileStorageProvider::new_with_storage_features(
                    input,
                    ::revm::context_interface::cfg::GasParams::default(),
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use alloy_evm::{
        EvmInternals,
        eth::EthEvmContext,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{BasePrecompileError, Result, StorageFeatures};
    use revm::{database::EmptyDB, precompile::PrecompileHalt, primitives::hardfork::SpecId};

    use crate::{B20Factory, IB20Factory, IPolicyRegistry, NoopPrecompileCallObserver};

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

    #[test]
    fn base_precompile_rejects_unaffordable_calldata_before_dispatch() {
        let dispatched = Arc::new(AtomicBool::new(false));
        let dispatched_by_precompile = Arc::clone(&dispatched);
        let precompile = base_precompile!(
            "test",
            storage_features: StorageFeatures::Legacy,
            |ctx, _calldata| {
                dispatched_by_precompile.store(true, Ordering::SeqCst);
                ctx.result_output(Ok(()), |_| Bytes::new())
            },
        );
        let calldata = vec![0xff; 119_972];
        let address = Address::repeat_byte(0x42);
        let mut evm = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);

        let output = precompile
            .call(PrecompileInput {
                data: &calldata,
                gas: 0,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut evm),
            })
            .unwrap();

        assert_eq!(output.halt_reason(), Some(&PrecompileHalt::OutOfGas));
        assert!(!dispatched.load(Ordering::SeqCst));
    }

    #[test]
    fn base_precompile_preserves_nonpayable_precedence_over_calldata_gas() {
        let precompile =
            B20Factory::precompile_with_observer(BaseUpgrade::Beryl, NoopPrecompileCallObserver);
        let calldata = vec![0xff; 119_972];
        let address = Address::repeat_byte(0x42);
        let mut evm = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);

        let output = precompile
            .call(PrecompileInput {
                data: &calldata,
                gas: 0,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::from(1),
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut evm),
            })
            .unwrap();

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(IB20Factory::NonPayable {}.abi_encode()));
    }
}
