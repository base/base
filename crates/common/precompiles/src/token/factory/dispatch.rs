//! ABI dispatch for the `TokenFactory` precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::storage::{
    TokenFactory, compute_default_address, compute_security_address, compute_stablecoin_address,
};
use crate::token::abi::ITokenFactory;

impl<'a> TokenFactory<'a> {
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
        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        match ITokenFactory::ITokenFactoryCalls::abi_decode(calldata) {
            Ok(ITokenFactory::ITokenFactoryCalls::createDefault(call)) => {
                let caller = ctx.caller();
                let token = self.create_default(caller, call)?;
                Ok(ITokenFactory::createDefaultCall::abi_encode_returns(&token).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::predictDefaultAddress(call)) => {
                let (addr, _) = compute_default_address(call.creator, call.salt);
                Ok(ITokenFactory::predictDefaultAddressCall::abi_encode_returns(&addr).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::predictStablecoinAddress(call)) => {
                let (addr, _) = compute_stablecoin_address(call.creator, call.salt);
                Ok(ITokenFactory::predictStablecoinAddressCall::abi_encode_returns(&addr).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::predictSecurityAddress(call)) => {
                let (addr, _) = compute_security_address(call.creator, call.salt);
                Ok(ITokenFactory::predictSecurityAddressCall::abi_encode_returns(&addr).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::isB20(call)) => {
                let result = self.is_b20(call.token)?;
                Ok(ITokenFactory::isB20Call::abi_encode_returns(&result).into())
            }
            Ok(ITokenFactory::ITokenFactoryCalls::variantOf(call)) => {
                let v = self.variant_of_token(call.token)?;
                Ok(ITokenFactory::variantOfCall::abi_encode_returns(&v).into())
            }
            Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
        }
    }
}
