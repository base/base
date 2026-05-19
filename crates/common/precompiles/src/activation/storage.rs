//! Storage layout and constants for the activation registry.

use alloy_primitives::{Address, B256, Bytes, address, b256};
use alloy_sol_types::SolEvent as _;
use base_precompile_macros::contract;
use base_precompile_storage::{
    BasePrecompileError, Handler, IntoPrecompileResult, Mapping, Result, StorageCtx,
};
use revm::precompile::PrecompileResult;

use super::IActivationRegistry;

/// Runtime activation registry for Base-native features.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Activation registry precompile address.
    pub const ADDRESS: Address = address!("0x84530000000000000000000000000000000000ff");

    /// Temporary activation admin address.
    ///
    /// Replace this with the final Base-controlled activation signer before deployment. The admin is
    /// protocol configuration: changing it after deployment requires a coordinated binary upgrade.
    pub const ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");

    /// Security-token factory creation feature id.
    pub const SECURITIES_TOKEN_CREATION: B256 =
        b256!("0x89e4523f0886ce01d76094212ed707081da92a45221e22c15c5689be470db63e");
}

/// Storage layout for the activation registry.
#[contract(addr = ActivationRegistry::ADDRESS)]
pub struct ActivationRegistryStorage {
    /// Runtime activation flags keyed by feature id.
    pub features: Mapping<B256, bool>,
}

impl ActivationRegistry {
    /// Creates a new activation registry handle.
    pub const fn new() -> Self {
        Self
    }

    /// Returns the activation admin.
    pub const fn admin(self) -> Address {
        Self::ADMIN
    }

    /// Returns true when the feature is activated.
    pub fn is_activated(self, ctx: StorageCtx<'_>, feature: B256) -> Result<bool> {
        ActivationRegistryStorage::new(ctx).features.at(&feature).read()
    }

    /// Reverts unless the feature is activated.
    ///
    /// Both the activated and deactivated paths return `Ok`; callers must inspect
    /// [`revm::precompile::PrecompileOutput::reverted`] to distinguish an activated feature from an
    /// ABI revert.
    pub fn assert_activated(self, ctx: StorageCtx<'_>, feature: B256) -> PrecompileResult {
        self.ensure_activated(ctx, feature)
            .into_precompile_result(ctx.gas_used(), |()| Bytes::new())
    }

    /// Returns `Ok(())` when the feature is activated.
    pub fn ensure_activated(self, ctx: StorageCtx<'_>, feature: B256) -> Result<()> {
        if self.is_activated(ctx, feature)? {
            return Ok(());
        }

        Err(BasePrecompileError::revert(IActivationRegistry::FeatureNotActivated { feature }))
    }

    /// Activates the feature.
    pub fn activate(self, ctx: StorageCtx<'_>, feature: B256) -> Result<()> {
        self.set_activated(ctx, feature, true)
    }

    /// Deactivates the feature.
    pub fn deactivate(self, ctx: StorageCtx<'_>, feature: B256) -> Result<()> {
        self.set_activated(ctx, feature, false)
    }

    /// Sets the feature activation state.
    pub fn set_activated(self, ctx: StorageCtx<'_>, feature: B256, activated: bool) -> Result<()> {
        // Keep this guard at the shared mutation boundary so `activate`, `deactivate`, and direct
        // `set_activated` callers all get the same static-call behavior after calldata validation.
        if ctx.is_static() {
            return Err(BasePrecompileError::revert(IActivationRegistry::StaticCallNotAllowed {}));
        }

        let caller = ctx.caller();
        if caller != Self::ADMIN {
            return Err(BasePrecompileError::revert(IActivationRegistry::Unauthorized { caller }));
        }

        let mut storage = ActivationRegistryStorage::new(ctx);
        let current = storage.features.at(&feature).read()?;
        if current == activated {
            if activated {
                return Err(BasePrecompileError::revert(IActivationRegistry::AlreadyActivated {
                    feature,
                }));
            }

            return Err(BasePrecompileError::revert(IActivationRegistry::AlreadyDeactivated {
                feature,
            }));
        }

        if activated {
            storage.features.at_mut(&feature).write(true)?;
            ctx.emit_event(
                Self::ADDRESS,
                IActivationRegistry::FeatureActivated { feature, caller }.encode_log_data(),
            )?;
        } else {
            storage.features.at_mut(&feature).delete()?;
            ctx.emit_event(
                Self::ADDRESS,
                IActivationRegistry::FeatureDeactivated { feature, caller }.encode_log_data(),
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, address};
    use base_precompile_storage::{HashMapStorageProvider, Result, StorageCtx};
    use revm::precompile::PrecompileOutput;
    use rstest::rstest;

    use super::*;

    const FEATURE: B256 = ActivationRegistry::SECURITIES_TOKEN_CREATION;

    #[derive(Debug, Clone, Copy)]
    enum Transition {
        Activate,
        Deactivate,
    }

    #[derive(Debug, Clone, Copy)]
    enum InvalidContext {
        Static,
        Unauthorized,
    }

    fn apply_transition(
        storage: &mut HashMapStorageProvider,
        transition: Transition,
    ) -> Result<()> {
        match transition {
            Transition::Activate => activate_feature(storage),
            Transition::Deactivate => deactivate_feature(storage),
        }
    }

    fn apply_transition_with_current_context(
        storage: &mut HashMapStorageProvider,
        transition: Transition,
    ) -> Result<()> {
        StorageCtx::enter(storage, |ctx| match transition {
            Transition::Activate => ActivationRegistry::new().activate(ctx, FEATURE),
            Transition::Deactivate => ActivationRegistry::new().deactivate(ctx, FEATURE),
        })
    }

    fn set_active(storage: &mut HashMapStorageProvider, active: bool) {
        if active {
            activate_feature(storage).unwrap();
        }
    }

    fn set_invalid_context(storage: &mut HashMapStorageProvider, context: InvalidContext) {
        match context {
            InvalidContext::Static => {
                storage.set_caller(ActivationRegistry::ADMIN);
                storage.set_static(true);
            }
            InvalidContext::Unauthorized => {
                storage.set_caller(address!("0x0000000000000000000000000000000000000001"));
            }
        }
    }

    fn activate_feature(storage: &mut HashMapStorageProvider) -> Result<()> {
        storage.set_caller(ActivationRegistry::ADMIN);
        StorageCtx::enter(storage, |ctx| ActivationRegistry::new().activate(ctx, FEATURE))
    }

    fn deactivate_feature(storage: &mut HashMapStorageProvider) -> Result<()> {
        storage.set_caller(ActivationRegistry::ADMIN);
        StorageCtx::enter(storage, |ctx| ActivationRegistry::new().deactivate(ctx, FEATURE))
    }

    fn assert_activated(storage: &mut HashMapStorageProvider, expected: bool) {
        StorageCtx::enter(storage, |ctx| {
            assert_eq!(
                ActivationRegistry::new()
                    .is_activated(ctx, FEATURE)
                    .expect("storage read succeeds"),
                expected
            );
        });
    }

    fn assert_activated_output(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        StorageCtx::enter(storage, |ctx| ActivationRegistry::new().assert_activated(ctx, FEATURE))
            .expect("activation assertion should not fail fatally")
    }

    #[test]
    fn feature_is_inactive_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        assert_activated(&mut storage, false);
    }

    #[test]
    fn admin_can_activate_deactivate_and_reactivate_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ActivationRegistry::ADDRESS).len(), 1);

        deactivate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, false);
        assert_eq!(storage.get_events(ActivationRegistry::ADDRESS).len(), 2);

        activate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ActivationRegistry::ADDRESS).len(), 3);
    }

    #[rstest]
    #[case::activate_when_active(Transition::Activate, true)]
    #[case::deactivate_when_inactive(Transition::Deactivate, false)]
    fn repeated_transition_reverts(#[case] transition: Transition, #[case] initially_active: bool) {
        let mut storage = HashMapStorageProvider::new(1);

        set_active(&mut storage, initially_active);
        let result = apply_transition(&mut storage, transition);

        assert!(result.is_err());
        assert_activated(&mut storage, initially_active);
    }

    #[rstest]
    #[case::activate_unauthorized(Transition::Activate, InvalidContext::Unauthorized, false)]
    #[case::deactivate_unauthorized(Transition::Deactivate, InvalidContext::Unauthorized, true)]
    #[case::activate_static(Transition::Activate, InvalidContext::Static, false)]
    #[case::deactivate_static(Transition::Deactivate, InvalidContext::Static, true)]
    fn invalid_context_cannot_change_activation(
        #[case] transition: Transition,
        #[case] context: InvalidContext,
        #[case] initially_active: bool,
    ) {
        let mut storage = HashMapStorageProvider::new(1);

        set_active(&mut storage, initially_active);
        set_invalid_context(&mut storage, context);
        let result = apply_transition_with_current_context(&mut storage, transition);

        assert!(result.is_err());
        assert_activated(&mut storage, initially_active);
    }

    #[test]
    fn assert_activated_reverts_after_deactivate() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage).unwrap();
        let activated_output = assert_activated_output(&mut storage);
        deactivate_feature(&mut storage).unwrap();
        let deactivated_output = assert_activated_output(&mut storage);

        assert!(!activated_output.reverted);
        assert!(deactivated_output.reverted);
    }
}
