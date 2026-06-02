//! ABI dispatch for the `B20Factory` precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    B20FactoryStorage, B20Variant, IB20Factory,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl<'a> B20FactoryStorage<'a> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Factory` handler.
    ///
    /// View (read-only) calls bypass the activation gate and remain accessible even when the
    /// feature is disabled. Write calls require the feature to be activated.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        let result = self.inner(ctx, calldata);
        let gas = ctx.gas_used();
        result.into_precompile_result(gas, ctx.state_gas_used(), |b| b)
    }

    fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        if !Self::is_view_selector(calldata) {
            ActivationRegistryStorage::new(ctx)
                .ensure_activated(ActivationFeature::B20Factory.id())?;
        }

        match decode_precompile_call!(calldata, IB20Factory::IB20FactoryCalls) {
            IB20Factory::IB20FactoryCalls::createB20(call) => {
                let caller = ctx.caller();
                let token = self.create_b20(caller, call)?;
                Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
            }
            IB20Factory::IB20FactoryCalls::getB20Address(call) => {
                let variant = B20Variant::from_abi(call.variant)
                    .ok_or_else(|| BasePrecompileError::revert(IB20Factory::InvalidVariant {}))?;
                let (addr, _) = variant.compute_address(call.sender, call.salt);
                Ok(IB20Factory::getB20AddressCall::abi_encode_returns(&addr).into())
            }
            IB20Factory::IB20FactoryCalls::isB20(call) => {
                let result = self.is_b20(call.token)?;
                Ok(IB20Factory::isB20Call::abi_encode_returns(&result).into())
            }
            IB20Factory::IB20FactoryCalls::isB20Initialized(call) => {
                let initialized = self.is_b20_initialized(call.token)?;
                Ok(IB20Factory::isB20InitializedCall::abi_encode_returns(&initialized).into())
            }
        }
    }

    /// Returns `true` when the calldata selector belongs to a view (read-only) function.
    ///
    /// View functions are accessible regardless of whether the feature is activated.
    fn is_view_selector(calldata: &[u8]) -> bool {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return false;
        };
        selector == IB20Factory::getB20AddressCall::SELECTOR
            || selector == IB20Factory::isB20Call::SELECTOR
            || selector == IB20Factory::isB20InitializedCall::SELECTOR
    }
}
