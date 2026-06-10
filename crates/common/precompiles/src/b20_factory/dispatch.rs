//! ABI dispatch for the `B20Factory` precompile.

use alloy_primitives::Bytes;
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
        let mut recorder =
            BerylCallRecorder::start(observer.clone(), BerylMetricLabels::factory_call(calldata));
        if !ctx.call_value().is_zero() {
            return recorder.record_base_error_result(
                ctx,
                BasePrecompileError::revert(IB20Factory::NonPayable {}),
            );
        }
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
                // abi_decode_validate rejects non-canonical discriminants before dispatch,
                // so from_abi returning None here would be an internal invariant violation.
                let variant = B20Variant::from_abi(call.variant).expect(
                    "abi_decode_validate rejects non-canonical discriminants before dispatch",
                );
                let address_hash = ctx.metered_keccak256(&(caller, call.salt).abi_encode())?;
                let token = self.create_b20_with_observer(call, address_hash, observer.clone())?;
                observer.record_b20_created(variant.as_label());
                Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
            }
            IB20Factory::IB20FactoryCalls::getB20Address(call) => {
                let v = B20Variant::from_abi(call.variant).expect(
                    "abi_decode_validate rejects non-canonical discriminants before dispatch",
                );
                let hash = ctx.metered_keccak256(&(call.sender, call.salt).abi_encode())?;
                let addr = v.compute_address_from_hash(hash).0;
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use alloy_sol_types::{SolCall, SolError};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{B20FactoryStorage, IB20Factory};

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));
        let calldata = IB20Factory::isB20Call { token: Address::ZERO }.abi_encode();

        let out = StorageCtx::enter(&mut storage, |ctx| {
            B20FactoryStorage::new(ctx).dispatch(ctx, &calldata)
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(
            out.bytes,
            alloy_primitives::Bytes::from(IB20Factory::NonPayable {}.abi_encode())
        );
    }
}
