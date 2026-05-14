use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, Handler, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::abi::ICounter;

use super::Counter;

pub fn dispatch(counter: &mut Counter, calldata: &[u8]) -> PrecompileResult {
    let ctx = StorageCtx;
    inner(counter, calldata).into_precompile_result(ctx.gas_used(), |b| b)
}

fn inner(counter: &mut Counter, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
    if calldata.len() < 4 {
        return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    }
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();

    match ICounter::ICounterCalls::abi_decode(calldata) {
        Ok(ICounter::ICounterCalls::increment(_)) => {
            let cur = counter.count.read()?;
            let next = cur
                .checked_add(U256::from(1u64))
                .ok_or(BasePrecompileError::under_overflow())?;
            counter.count.write(next)?;
            Ok(Bytes::new())
        }
        Ok(ICounter::ICounterCalls::getCount(_)) => {
            let count = counter.count.read()?;
            Ok(ICounter::getCountCall::abi_encode_returns(&count).into())
        }
        Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
    }
}
