//! ABI dispatch for the `TokenFactory` precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    ActivationRegistryStorage, ITokenFactory, TokenFactoryStorage, TokenVariant,
    macros::decode_precompile_call,
};

impl<'a> TokenFactoryStorage<'a> {
    /// ABI-dispatches `calldata` to the appropriate `ITokenFactory` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        if let Err(e) = ctx.deduct_gas(crate::input_cost(calldata.len())) {
            return e.into_precompile_result(ctx.gas_used());
        }
        let result = self.inner(ctx, calldata);
        let gas = ctx.gas_used();
        result.into_precompile_result(gas, |b| b)
    }

    fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationRegistryStorage::TOKEN_FACTORY)?;

        match decode_precompile_call!(calldata, ITokenFactory::ITokenFactoryCalls) {
            ITokenFactory::ITokenFactoryCalls::createToken(call) => {
                let caller = ctx.caller();
                let token = self.create_token(caller, call)?;
                Ok(ITokenFactory::createTokenCall::abi_encode_returns(&token).into())
            }
            ITokenFactory::ITokenFactoryCalls::getTokenAddress(call) => {
                let Some(variant) = TokenFactoryStorage::token_variant(call.variant) else {
                    return Err(BasePrecompileError::revert(ITokenFactory::InvalidVariant {}));
                };
                let (addr, _) = variant.compute_address(call.sender, call.decimals, call.salt);
                Ok(ITokenFactory::getTokenAddressCall::abi_encode_returns(&addr).into())
            }
            ITokenFactory::ITokenFactoryCalls::isB20(call) => {
                let result = self.is_b20(call.token)?;
                Ok(ITokenFactory::isB20Call::abi_encode_returns(&result).into())
            }
            ITokenFactory::ITokenFactoryCalls::getTokenVariant(call) => {
                let variant =
                    TokenFactoryStorage::abi_variant(TokenVariant::from_address(call.token));
                Ok(ITokenFactory::getTokenVariantCall::abi_encode_returns(&variant).into())
            }
        }
    }
}
