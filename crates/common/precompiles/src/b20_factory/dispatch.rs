//! ABI dispatch for the `B20Factory` precompile.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::StorageCtx;
use revm::precompile::PrecompileResult;

use crate::{
    B20FactoryStorage, B20Variant, BerylCallRecorder, BerylMetricLabels, IB20Factory,
    NoopPrecompileCallObserver, PrecompileCallObserver, macros::decode_precompile_call,
};

impl<'a> B20FactoryStorage<'a> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Factory` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` to the appropriate `IB20Factory` handler with an observer.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder =
            BerylCallRecorder::start(observer.clone(), BerylMetricLabels::factory_call(calldata));
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        recorder.record_base_result(ctx, self.inner(ctx, calldata, observer), |b| b)
    }

    fn inner<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        match decode_precompile_call!(calldata, IB20Factory::IB20FactoryCalls) {
            IB20Factory::IB20FactoryCalls::createB20(call) => {
                let caller = ctx.caller();
                let variant = B20Variant::from_abi(call.variant);
                let token = self.create_b20_with_observer(caller, call, observer.clone())?;
                if let Some(variant) = variant {
                    observer.record_b20_created(variant.as_label());
                }
                Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
            }
            IB20Factory::IB20FactoryCalls::getB20Address(call) => {
                let addr = B20Variant::from_abi(call.variant)
                    .expect("abi_decode_validate rejects non-canonical discriminants")
                    .compute_address(call.sender, call.salt)
                    .0;
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
}
