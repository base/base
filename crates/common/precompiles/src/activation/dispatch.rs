//! ABI dispatch for the activation registry.

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::StorageCtx;
use revm::precompile::PrecompileResult;

use crate::{
    ActivationAdminConfig, ActivationRegistryStorage, BerylCallRecorder, BerylMetricLabels,
    IActivationRegistry::{self, IActivationRegistryCalls as C},
    NoopPrecompileCallObserver, PrecompileCallObserver,
    macros::decode_precompile_call,
};

impl ActivationRegistryStorage<'_> {
    /// ABI-dispatches activation registry calldata.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
    ) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, admin_config, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches activation registry calldata with an observer.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder =
            BerylCallRecorder::start(observer, BerylMetricLabels::activation_call(calldata));
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        recorder.record_base_result(ctx, self.inner(calldata, admin_config), |output| output)
    }

    fn inner(
        &mut self,
        calldata: &[u8],
        admin_config: ActivationAdminConfig,
    ) -> base_precompile_storage::Result<Bytes> {
        match decode_precompile_call!(calldata, IActivationRegistry::IActivationRegistryCalls) {
            C::isActivated(call) => {
                let activated = self.is_activated(call.feature)?;
                Ok(IActivationRegistry::isActivatedCall::abi_encode_returns(&activated).into())
            }
            C::checkActivated(call) => {
                self.ensure_activated(call.feature)?;
                Ok(Bytes::new())
            }
            C::activate(call) => {
                self.activate(call.feature, admin_config)?;
                Ok(Bytes::new())
            }
            C::deactivate(call) => {
                self.deactivate(call.feature, admin_config)?;
                Ok(Bytes::new())
            }
            C::setAdmin(call) => {
                self.set_admin(call.newAdmin, admin_config)?;
                Ok(Bytes::new())
            }
            C::admin(_) => {
                Ok(IActivationRegistry::adminCall::abi_encode_returns(&self.admin(admin_config)?)
                    .into())
            }
        }
    }
}
