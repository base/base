use base_common_genesis::{BaseFeature, BaseUpgrade};
use crate::StorageFeatures;

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
