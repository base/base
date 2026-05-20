//! ABI dispatch for the activation registry.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    ActivationRegistryStorage,
    IActivationRegistry::{self, IActivationRegistryCalls as C},
};

impl ActivationRegistryStorage<'_> {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        activation_admin_address: Option<Address>,
    ) -> PrecompileResult {
        if let Err(e) = ctx.deduct_gas(crate::input_cost(calldata.len())) {
            return e.into_precompile_result(ctx.gas_used());
        }
        self.inner(calldata, activation_admin_address)
            .into_precompile_result(ctx.gas_used(), |output| output)
    }

    fn inner(
        &mut self,
        calldata: &[u8],
        activation_admin_address: Option<Address>,
    ) -> base_precompile_storage::Result<Bytes> {
        if calldata.len() < 4 {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        }
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();

        match IActivationRegistry::IActivationRegistryCalls::abi_decode(calldata) {
            Ok(C::isActivated(call)) => {
                let activated = self.is_activated(call.feature)?;
                Ok(IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into())
            }
            Ok(C::activate(call)) => {
                self.activate(call.feature, activation_admin_address)?;
                Ok(Bytes::new())
            }
            Ok(C::deactivate(call)) => {
                self.deactivate(call.feature, activation_admin_address)?;
                Ok(Bytes::new())
            }
            Ok(C::admin(_)) => Ok(IActivationRegistry::adminCall::abi_encode_returns(
                &self.admin(activation_admin_address),
            )
            .into()),
            Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
        }
    }
}
