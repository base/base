use base_common_chain_config::BaseUpgrade;
use base_common_precompiles::StorageFeatures;

/// Resolves Base upgrades into fork-dependent persistent-storage features.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeGatedStorageFeatures;

impl UpgradeGatedStorageFeatures {
    /// Returns the persistent-storage features active at `upgrade`.
    pub fn from_upgrade(upgrade: BaseUpgrade) -> StorageFeatures {
        if upgrade >= BaseUpgrade::Cobalt {
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
