//! ABI dispatch for the activation registry.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    ActivationRegistryStorage,
    IActivationRegistry::{self, IActivationRegistryCalls as C},
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl ActivationRegistryStorage<'_> {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        activation_admin_address: Option<Address>,
    ) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        self.inner(calldata, activation_admin_address).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            |output| output,
        )
    }

    fn inner(
        &mut self,
        calldata: &[u8],
        activation_admin_address: Option<Address>,
    ) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, IActivationRegistry::IActivationRegistryCalls) {
            C::isActivated(call) => {
                let activated = self.is_activated(call.feature)?;
                Ok(IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into())
            }
            C::activate(call) => {
                self.activate(call.feature, activation_admin_address)?;
                Ok(Bytes::new())
            }
            C::deactivate(call) => {
                self.deactivate(call.feature, activation_admin_address)?;
                Ok(Bytes::new())
            }
            C::admin(_) => Ok(IActivationRegistry::adminCall::abi_encode_returns(
                &self.admin(activation_admin_address),
            )
            .into()),
        }
    }
}
