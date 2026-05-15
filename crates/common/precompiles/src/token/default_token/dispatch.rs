use alloy_primitives::Bytes;
use alloy_sol_types::SolInterface;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::token::abi::IDefaultToken;

use super::DefaultToken;

/// ABI-dispatches a raw calldata slice to the appropriate `IDefaultToken` function handler.
pub fn dispatch(pc: &mut DefaultToken, calldata: &[u8]) -> PrecompileResult {
    let ctx = StorageCtx;
    inner(pc, calldata).into_precompile_result(ctx.gas_used(), |b| b)
}

fn inner(_pc: &mut DefaultToken, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
    if calldata.len() < 4 {
        return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    }
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();

    match IDefaultToken::IDefaultTokenCalls::abi_decode(calldata) {
        Ok(_call) => todo!("implement dispatch for each IDefaultToken call variant"),
        Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
    }
}
