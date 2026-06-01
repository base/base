//! Storage layout and constants for the activation registry.

use alloy_primitives::{Address, B256, Bytes, address, b256};
use base_precompile_macros::contract;
use base_precompile_storage::{
    BasePrecompileError, Handler, IntoPrecompileResult, Mapping, Result,
};
use revm::precompile::PrecompileResult;

use crate::IActivationRegistry;

/// Runtime activation registry for Base-native features.
#[contract(addr = Self::ADDRESS)]
#[namespace("base.activation_registry")]
pub struct ActivationRegistryStorage {
    /// Runtime activation flags keyed by feature id.
    pub features: Mapping<B256, bool>,
}

/// Identifies a Base-native precompile feature in the activation registry.
///
/// Each variant maps to a stable `keccak256` hash of the feature's canonical name and is used as
/// the key when querying or mutating activation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationFeature {
    /// `keccak256("base.b20_token")`
    B20Token,
    /// `keccak256("base.b20_factory")`
    B20Factory,
    /// `keccak256("base.policy_registry")`
    PolicyRegistry,
    /// `keccak256("base.b20_stablecoin")`
    B20Stablecoin,
    /// `keccak256("base.b20_security")`
    B20Asset,
}

impl ActivationFeature {
    /// Returns the `keccak256` hash that identifies this feature in storage.
    pub const fn id(self) -> B256 {
        match self {
            Self::B20Token => {
                b256!("0x47a1afe8d3d691b87e090ee972d223a11f4da971ff5416c04985bb2393aca752")
            }
            Self::B20Factory => {
                b256!("0x78751e29c8bcc0d609ab18e9fbc4158e73f7db25ae2ee095dad42e2578b1e800")
            }
            Self::PolicyRegistry => {
                b256!("0xb582ebae03f16fee49a6763f78df482fb11ae73f103ed0d330bbe556aa90a43f")
            }
            Self::B20Stablecoin => {
                b256!("0xecfa0def2c10020caaf65e6155aa69c84b24892aaef76eeac52e0e2b3a0b8601")
            }
            Self::B20Asset => {
                b256!("0x83d32fab502ae0e8bc4352a117767262cb5e47cc8d67a744008ed4ff03fcf5e6")
            }
        }
    }
}

impl From<ActivationFeature> for B256 {
    fn from(feature: ActivationFeature) -> Self {
        feature.id()
    }
}

impl ActivationRegistryStorage<'_> {
    /// Activation registry precompile address.
    pub const ADDRESS: Address = address!("8453000000000000000000000000000000000001");

    /// Returns the activation admin.
    pub const fn admin(&self, activation_admin_address: Option<Address>) -> Address {
        match activation_admin_address {
            Some(address) => address,
            None => Address::ZERO,
        }
    }

    /// Returns true when the feature is activated.
    pub fn is_activated(&self, feature: B256) -> Result<bool> {
        self.features.at(&feature).read()
    }

    /// Reverts unless the feature is activated.
    ///
    /// Both the activated and deactivated paths return `Ok`; callers must inspect
    /// [`revm::precompile::PrecompileOutput::reverted`] to distinguish an activated feature from an
    /// ABI revert.
    pub fn assert_activated(&self, feature: B256) -> PrecompileResult {
        self.ensure_activated(feature).into_precompile_result(
            self.storage.gas_used(),
            self.storage.state_gas_used(),
            |()| Bytes::new(),
        )
    }

    /// Returns `Ok(())` when the feature is activated.
    pub fn ensure_activated(&self, feature: B256) -> Result<()> {
        if self.is_activated(feature)? {
            return Ok(());
        }

        Err(BasePrecompileError::revert(IActivationRegistry::FeatureNotActivated { feature }))
    }

    /// Activates the feature.
    pub fn activate(
        &mut self,
        feature: B256,
        activation_admin_address: Option<Address>,
    ) -> Result<()> {
        self.set_activated(feature, true, activation_admin_address)
    }

    /// Deactivates the feature.
    pub fn deactivate(
        &mut self,
        feature: B256,
        activation_admin_address: Option<Address>,
    ) -> Result<()> {
        self.set_activated(feature, false, activation_admin_address)
    }

    /// Sets the feature activation state.
    pub fn set_activated(
        &mut self,
        feature: B256,
        activated: bool,
        activation_admin_address: Option<Address>,
    ) -> Result<()> {
        // Keep this guard at the shared mutation boundary so `activate`, `deactivate`, and direct
        // `set_activated` callers all get the same static-call behavior after calldata validation.
        if self.storage.is_static() {
            return Err(BasePrecompileError::revert(IActivationRegistry::StaticCallNotAllowed {}));
        }

        let caller = self.storage.caller();
        let Some(admin) = activation_admin_address else {
            return Err(BasePrecompileError::revert(IActivationRegistry::Unauthorized { caller }));
        };
        if caller != admin {
            return Err(BasePrecompileError::revert(IActivationRegistry::Unauthorized { caller }));
        }

        let current = self.features.at(&feature).read()?;
        if current == activated {
            if activated {
                return Err(BasePrecompileError::revert(IActivationRegistry::AlreadyActivated {
                    feature,
                }));
            }

            return Err(BasePrecompileError::revert(IActivationRegistry::FeatureNotActivated {
                feature,
            }));
        }

        if activated {
            self.__initialize()?;
            self.features.at_mut(&feature).write(true)?;
            self.emit_event(IActivationRegistry::FeatureActivated { feature, caller })?;
        } else {
            self.features.at_mut(&feature).delete()?;
            self.emit_event(IActivationRegistry::FeatureDeactivated { feature, caller })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, address, keccak256, uint};
    use base_precompile_storage::{
        BasePrecompileError, HashMapStorageProvider, Result, StorageCtx, StorageKey,
    };
    use revm::precompile::PrecompileOutput;
    use rstest::rstest;

    use crate::{
        ActivationFeature, ActivationRegistryStorage, IActivationRegistry,
        activation::storage::slots,
    };

    const FEATURE: B256 = ActivationFeature::B20Asset.id();
    const ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const ACTIVATION_REGISTRY_ROOT: U256 =
        uint!(0x43ee1bbe25e988521cccd8b2c8fbd38c8287ebff8e074e825a70dfd3885cce00_U256);

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
        StorageCtx::enter(storage, |ctx| {
            let mut registry = ActivationRegistryStorage::new(ctx);
            match transition {
                Transition::Activate => registry.activate(FEATURE, Some(ADMIN)),
                Transition::Deactivate => registry.deactivate(FEATURE, Some(ADMIN)),
            }
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
                storage.set_caller(ADMIN);
                storage.set_static(true);
            }
            InvalidContext::Unauthorized => {
                storage.set_caller(address!("0x0000000000000000000000000000000000000001"));
            }
        }
    }

    fn activate_feature(storage: &mut HashMapStorageProvider) -> Result<()> {
        storage.set_caller(ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx).activate(FEATURE, Some(ADMIN))
        })
    }

    fn deactivate_feature(storage: &mut HashMapStorageProvider) -> Result<()> {
        storage.set_caller(ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx).deactivate(FEATURE, Some(ADMIN))
        })
    }

    fn assert_activated(storage: &mut HashMapStorageProvider, expected: bool) {
        StorageCtx::enter(storage, |ctx| {
            assert_eq!(
                ActivationRegistryStorage::new(ctx)
                    .is_activated(FEATURE)
                    .expect("storage read succeeds"),
                expected
            );
        });
    }

    fn assert_activated_output(storage: &mut HashMapStorageProvider) -> PrecompileOutput {
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx).assert_activated(FEATURE)
        })
        .expect("activation assertion should not fail fatally")
    }

    #[test]
    fn feature_is_inactive_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        assert_activated(&mut storage, false);
    }

    #[test]
    fn feature_id_constants_match_canonical_names() {
        assert_eq!(ActivationFeature::B20Token.id(), keccak256("base.b20_token"));
        assert_eq!(ActivationFeature::B20Factory.id(), keccak256("base.b20_factory"));
        assert_eq!(ActivationFeature::PolicyRegistry.id(), keccak256("base.policy_registry"));
        assert_eq!(ActivationFeature::B20Stablecoin.id(), keccak256("base.b20_stablecoin"));
        assert_eq!(ActivationFeature::B20Asset.id(), keccak256("base.b20_security"));
    }

    #[test]
    fn activation_registry_namespace_matches_base_std_root() {
        assert_eq!(slots::NAMESPACE_ID, "base.activation_registry");
        assert_eq!(slots::NAMESPACE_ROOT, ACTIVATION_REGISTRY_ROOT);
        assert_eq!(slots::FEATURES, ACTIVATION_REGISTRY_ROOT);
    }

    #[test]
    fn activation_registry_writes_use_base_std_namespace_slots() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage).unwrap();

        StorageCtx::enter(&mut storage, |ctx| {
            assert_eq!(
                ctx.sload(
                    ActivationRegistryStorage::ADDRESS,
                    FEATURE.mapping_slot(slots::FEATURES)
                )
                .unwrap(),
                U256::ONE
            );
            assert_eq!(
                ctx.sload(ActivationRegistryStorage::ADDRESS, FEATURE.mapping_slot(U256::ZERO))
                    .unwrap(),
                U256::ZERO
            );
        });
    }

    #[test]
    fn admin_can_activate_deactivate_and_reactivate_feature() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ActivationRegistryStorage::ADDRESS).len(), 1);

        deactivate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, false);
        assert_eq!(storage.get_events(ActivationRegistryStorage::ADDRESS).len(), 2);

        activate_feature(&mut storage).unwrap();
        assert_activated(&mut storage, true);
        assert_eq!(storage.get_events(ActivationRegistryStorage::ADDRESS).len(), 3);
    }

    #[test]
    fn configured_admin_can_activate_when_default_is_unset() {
        let mut storage = HashMapStorageProvider::new(1);
        let configured_admin = address!("0x0000000000000000000000000000000000000002");

        storage.set_caller(ADMIN);
        let err = StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistryStorage::new(ctx).activate(FEATURE, Some(configured_admin))
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
        assert_activated(&mut storage, false);

        storage.set_caller(configured_admin);
        StorageCtx::enter(&mut storage, |ctx| {
            ActivationRegistryStorage::new(ctx).activate(FEATURE, Some(configured_admin))
        })
        .unwrap();
        assert_activated(&mut storage, true);
    }

    #[test]
    fn unset_admin_cannot_change_activation() {
        let mut storage = HashMapStorageProvider::new(1);

        storage.set_caller(ADMIN);
        let err = StorageCtx::enter(&mut storage, |ctx| {
            let mut registry = ActivationRegistryStorage::new(ctx);
            assert_eq!(registry.admin(None), Address::ZERO);
            registry.activate(FEATURE, None)
        })
        .unwrap_err();

        assert!(matches!(err, BasePrecompileError::Revert(_)));
        assert_activated(&mut storage, false);
    }

    #[rstest]
    #[case::activate_when_active(Transition::Activate, true)]
    #[case::deactivate_when_inactive(Transition::Deactivate, false)]
    fn repeated_transition_reverts(#[case] transition: Transition, #[case] initially_active: bool) {
        let mut storage = HashMapStorageProvider::new(1);

        set_active(&mut storage, initially_active);
        let events_before = storage.get_events(ActivationRegistryStorage::ADDRESS).len();

        let result = apply_transition(&mut storage, transition);

        assert_eq!(
            result.unwrap_err(),
            match transition {
                Transition::Activate => {
                    BasePrecompileError::revert(IActivationRegistry::AlreadyActivated {
                        feature: FEATURE,
                    })
                }
                Transition::Deactivate => {
                    BasePrecompileError::revert(IActivationRegistry::FeatureNotActivated {
                        feature: FEATURE,
                    })
                }
            }
        );
        assert_activated(&mut storage, initially_active);
        // A failed transition must not emit any events — guard against emit-then-revert bugs.
        assert_eq!(storage.get_events(ActivationRegistryStorage::ADDRESS).len(), events_before);
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
    fn assert_activated_reverts_when_feature_never_activated() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = assert_activated_output(&mut storage);

        assert!(output.is_revert());
        assert_eq!(storage.get_events(ActivationRegistryStorage::ADDRESS).len(), 0);
    }

    #[test]
    fn assert_activated_reverts_after_deactivate() {
        let mut storage = HashMapStorageProvider::new(1);

        activate_feature(&mut storage).unwrap();
        let activated_output = assert_activated_output(&mut storage);
        deactivate_feature(&mut storage).unwrap();
        let deactivated_output = assert_activated_output(&mut storage);

        assert!(!activated_output.is_revert());
        assert!(deactivated_output.is_revert());
    }
}
