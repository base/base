//! ABI dispatch for the `B20Factory` precompile.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolValue};
use base_precompile_storage::{BasePrecompileError, StorageCtx};
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
        // All factory selectors are nonpayable: reject any call that attaches ETH.
        // The guard fires before calldata-cost deduction so value-bearing calls pay
        // zero gas before reverting, matching Solidity's nonpayable semantics.
        if !ctx.call_value().is_zero() {
            return ctx.error_result(BasePrecompileError::revert(IB20Factory::NonPayable {}));
        }
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
                // Charge keccak gas for address derivation only when the variant is valid.
                // Invalid variants are rejected in create_b20_with_observer before
                // compute_address runs, so no keccak hash occurs on the revert path.
                // compute_address re-encodes (caller, salt) internally to stay ctx-free;
                // the resulting double allocation is intentional.
                if variant.is_some() {
                    ctx.keccak256(&(caller, call.salt).abi_encode())?;
                }
                let token = self.create_b20_with_observer(caller, call, observer.clone())?;
                if let Some(variant) = variant {
                    observer.record_b20_created(variant.as_label());
                }
                Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
            }
            IB20Factory::IB20FactoryCalls::getB20Address(call) => {
                // Returns zero for an unrecognized variant to match base-std, which documents
                // this function as "Never reverts" (meaning no ABI revert; keccak gas is
                // still charged for recognized variants and OOG can terminate the call frame).
                let addr = match B20Variant::from_abi(call.variant) {
                    Some(v) => {
                        ctx.keccak256(&(call.sender, call.salt).abi_encode())?;
                        v.compute_address(call.sender, call.salt).0
                    }
                    None => Address::ZERO,
                };
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
