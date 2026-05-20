//! ABI dispatch for the `TokenFactory` precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{ActivationRegistryStorage, ITokenFactory, TokenFactoryStorage, TokenVariant};

impl<'a> TokenFactoryStorage<'a> {
    /// ABI-dispatches `calldata` to the appropriate `ITokenFactory` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
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

        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        match ITokenFactory::ITokenFactoryCalls::abi_decode(calldata) {
            Ok(ITokenFactory::ITokenFactoryCalls::createToken(call)) => {
                let caller = ctx.caller();
                let token = self.create_token(caller, call)?;
                Ok(ITokenFactory::createTokenCall::abi_encode_returns(&token).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::predictTokenAddress(call)) => {
                let (addr, _) = TokenVariant::compute_address_for_discriminant(
                    call.creator,
                    call.variant,
                    call.decimals,
                    call.salt,
                );
                Ok(ITokenFactory::predictTokenAddressCall::abi_encode_returns(&addr).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::isB20(call)) => {
                let result = self.is_b20(call.token)?;
                Ok(ITokenFactory::isB20Call::abi_encode_returns(&result).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::variantOf(call)) => {
                let v = self.variant_of_token(call.token)?;
                Ok(ITokenFactory::variantOfCall::abi_encode_returns(&v).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::decimalsOf(call)) => {
                let decimals = self.decimals_of_token(call.token)?;
                Ok(ITokenFactory::decimalsOfCall::abi_encode_returns(&decimals).into())
            }
            Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
        }
    }
}
