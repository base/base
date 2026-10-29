use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_precompile_storage::StorageFeatures;

// The two-profile storage API requires these distinct behaviors to activate together. If a
// future upgrade separates them, extend StorageFeatures before changing either registry entry.
const _: () = assert!(
    BaseFeature::DynamicStorageTailCleanup.activation_upgrade() as u8
        == BaseFeature::CompactAbiDecodeErrors.activation_upgrade() as u8,
    "separate storage feature activations require separate storage profiles",
);

/// Resolves Base upgrades into fork-dependent persistent-storage features.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeGatedStorageFeatures;

impl UpgradeGatedStorageFeatures {
    /// Returns the persistent-storage features active at `upgrade`.
    pub const fn from_upgrade(upgrade: BaseUpgrade) -> StorageFeatures {
        if upgrade.is_feature_enabled(BaseFeature::DynamicStorageTailCleanup) {
            StorageFeatures::Cobalt
        } else {
            StorageFeatures::Legacy
        }
    }
}

/// A chain spec that can select Base precompile sets.
pub trait BasePrecompileSpec: Copy + Eq + From<BaseUpgrade> + Into<BaseUpgrade> {
    /// Returns the default precompile spec.
    fn default_precompile_spec() -> Self {
        BaseUpgrade::LATEST.into()
    }

    /// Returns the Base upgrade associated with this spec.
    fn upgrade(self) -> BaseUpgrade {
        self.into()
    }
}

impl<S> BasePrecompileSpec for S where S: Copy + Eq + From<BaseUpgrade> + Into<BaseUpgrade> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_features_activate_at_cobalt() {
        assert_eq!(
            UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Beryl),
            StorageFeatures::Legacy,
        );
        assert_eq!(
            UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Cobalt),
            StorageFeatures::Cobalt,
        );
    }
}
