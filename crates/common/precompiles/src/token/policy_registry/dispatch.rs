use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::storage::PolicyRegistryStorage;
use crate::token::abi::{IPolicyRegistry, IPolicyRegistry::IPolicyRegistryCalls as C};

impl PolicyRegistryStorage<'_> {
    /// ABI-dispatches `calldata` to the appropriate `IPolicyRegistry` handler.
    pub(super) fn dispatch(&self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.inner(calldata).into_precompile_result(ctx.gas_used(), |b| b)
    }

    fn inner(&self, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        match IPolicyRegistry::IPolicyRegistryCalls::abi_decode(calldata) {
            Ok(C::helloWorld(_)) => {
                Ok(IPolicyRegistry::helloWorldCall::abi_encode_returns(&true).into())
            }
            Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
        }
    }
}
