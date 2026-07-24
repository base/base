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

/// Emits a fully-delegating [`Asset`](crate::Asset) impl that forwards each listed method to a
/// prior version.
///
/// Every precompile version is a distinct, frozen [`Asset`](crate::Asset) implementation. A
/// version that only *adds* behavior would otherwise restate the entire inherited surface just to
/// forward it to the version it extends. The macro generates that forwarding from a single
/// method-name list, so a version's module contains only the methods whose behavior diverges.
///
/// `delegate_asset!(NewVersion => prior, { method_a, ... })` forwards each named method to `prior`
/// (a unit-struct value such as [`AssetV1`](crate::AssetV1)). An overridden method is omitted from
/// the list and given its real implementation instead. Rust allows only one `impl Asset for` a
/// given type, so an override cannot live in a second `impl` block; the macro gains an
/// `overrides { ... }` section (spliced into this generated `impl`) in the follow-up PR that
/// introduces the first diverging version.
macro_rules! delegate_asset {
    ($target:ty => $to:expr, { $($method:ident),+ $(,)? }) => {
        impl<S: $crate::AssetAccounting, A: $crate::PolicyAccounting> $crate::Asset<S, A>
            for $target
        {
            $($crate::macros::delegate_asset!(@fwd $to, $method);)+
        }
    };

    (@fwd $to:expr, transfer) => {
        fn transfer(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.transfer(token, caller, to, amount, privileged)
        }
    };
    (@fwd $to:expr, transfer_from) => {
        fn transfer_from(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            from: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.transfer_from(token, caller, from, to, amount, privileged)
        }
    };
    (@fwd $to:expr, approve) => {
        fn approve(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            spender: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.approve(token, caller, spender, amount)
        }
    };
    (@fwd $to:expr, emit_memo) => {
        fn emit_memo(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            memo: ::alloy_primitives::B256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.emit_memo(token, caller, memo)
        }
    };
    (@fwd $to:expr, mint) => {
        fn mint(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.mint(token, caller, to, amount, privileged)
        }
    };
    (@fwd $to:expr, burn) => {
        fn burn(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.burn(token, caller, amount)
        }
    };
    (@fwd $to:expr, burn_blocked) => {
        fn burn_blocked(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            from: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.burn_blocked(token, caller, from, amount, privileged)
        }
    };
    (@fwd $to:expr, pause) => {
        fn pause(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            features: ::alloc::vec::Vec<$crate::IB20::PausableFeature>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.pause(token, caller, features, privileged)
        }
    };
    (@fwd $to:expr, unpause) => {
        fn unpause(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            features: ::alloc::vec::Vec<$crate::IB20::PausableFeature>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.unpause(token, caller, features, privileged)
        }
    };
    (@fwd $to:expr, update_supply_cap) => {
        fn update_supply_cap(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            new_cap: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_supply_cap(token, caller, new_cap, privileged)
        }
    };
    (@fwd $to:expr, update_name) => {
        fn update_name(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            name: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_name(token, caller, name, privileged)
        }
    };
    (@fwd $to:expr, update_symbol) => {
        fn update_symbol(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            symbol: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_symbol(token, caller, symbol, privileged)
        }
    };
    (@fwd $to:expr, update_contract_uri) => {
        fn update_contract_uri(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            uri: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_contract_uri(token, caller, uri, privileged)
        }
    };
    (@fwd $to:expr, grant_role) => {
        fn grant_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            account: ::alloy_primitives::Address,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.grant_role(token, caller, role, account, privileged)
        }
    };
    (@fwd $to:expr, revoke_role) => {
        fn revoke_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            account: ::alloy_primitives::Address,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.revoke_role(token, caller, role, account, privileged)
        }
    };
    (@fwd $to:expr, renounce_role) => {
        fn renounce_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            confirmation: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<()> {
            $to.renounce_role(token, caller, role, confirmation)
        }
    };
    (@fwd $to:expr, renounce_last_admin) => {
        fn renounce_last_admin(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<()> {
            $to.renounce_last_admin(token, caller)
        }
    };
    (@fwd $to:expr, set_role_admin) => {
        fn set_role_admin(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            new_admin_role: ::alloy_primitives::B256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.set_role_admin(token, caller, role, new_admin_role, privileged)
        }
    };
    (@fwd $to:expr, update_policy) => {
        fn update_policy(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            policy_scope: ::alloy_primitives::B256,
            new_policy_id: u64,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_policy(token, caller, policy_scope, new_policy_id, privileged)
        }
    };
    (@fwd $to:expr, permit) => {
        fn permit(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            chain_id: u64,
            now: ::alloy_primitives::U256,
            args: $crate::PermitArgs,
        ) -> ::base_precompile_storage::Result<()> {
            $to.permit(token, chain_id, now, args)
        }
    };
    (@fwd $to:expr, update_multiplier) => {
        fn update_multiplier(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            new_multiplier: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_multiplier(token, caller, new_multiplier, privileged)
        }
    };
    (@fwd $to:expr, update_extra_metadata) => {
        fn update_extra_metadata(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            key: ::alloc::string::String,
            value: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_extra_metadata(token, caller, key, value, privileged)
        }
    };
    (@fwd $to:expr, batch_mint) => {
        fn batch_mint(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            recipients: ::alloc::vec::Vec<::alloy_primitives::Address>,
            amounts: ::alloc::vec::Vec<::alloy_primitives::U256>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.batch_mint(token, caller, recipients, amounts, privileged)
        }
    };
    (@fwd $to:expr, begin_announce) => {
        fn begin_announce(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            id: ::alloc::string::String,
            description: ::alloc::string::String,
            uri: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.begin_announce(token, caller, id, description, uri, privileged)
        }
    };
    (@fwd $to:expr, end_announce) => {
        fn end_announce(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            id: ::alloc::string::String,
        ) -> ::base_precompile_storage::Result<()> {
            $to.end_announce(token, id)
        }
    };
    (@fwd $to:expr, is_paused) => {
        fn is_paused(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            feature: $crate::IB20::PausableFeature,
        ) -> ::base_precompile_storage::Result<bool> {
            $to.is_paused(token, feature)
        }
    };
    (@fwd $to:expr, paused_features) => {
        fn paused_features(
            &self,
            token: &$crate::B20AssetToken<S, A>,
        ) -> ::base_precompile_storage::Result<::alloc::vec::Vec<$crate::IB20::PausableFeature>> {
            $to.paused_features(token)
        }
    };
    (@fwd $to:expr, policy_id) => {
        fn policy_id(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            policy_scope: ::alloy_primitives::B256,
        ) -> ::base_precompile_storage::Result<u64> {
            $to.policy_id(token, policy_scope)
        }
    };
    (@fwd $to:expr, domain_separator) => {
        fn domain_separator(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            chain_id: u64,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::B256> {
            $to.domain_separator(token, chain_id)
        }
    };
    (@fwd $to:expr, eip712_domain) => {
        fn eip712_domain(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            chain_id: u64,
        ) -> ::base_precompile_storage::Result<$crate::Eip712Domain> {
            $to.eip712_domain(token, chain_id)
        }
    };
    (@fwd $to:expr, to_scaled_balance) => {
        fn to_scaled_balance(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            balance: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.to_scaled_balance(token, balance)
        }
    };
    (@fwd $to:expr, to_raw_balance) => {
        fn to_raw_balance(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            balance: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.to_raw_balance(token, balance)
        }
    };
    (@fwd $to:expr, scaled_balance_of) => {
        fn scaled_balance_of(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            account: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.scaled_balance_of(token, account)
        }
    };
    (@fwd $to:expr, operator_role) => {
        fn operator_role(&self) -> ::alloy_primitives::B256 {
            // Unlike the token-taking methods, this signature never mentions `S`/`A`, so the
            // delegate's generic params cannot be inferred from an argument. Name the trait
            // instantiation explicitly to disambiguate.
            $crate::Asset::<S, A>::operator_role(&$to)
        }
    };
}

pub(crate) use delegate_asset;

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
