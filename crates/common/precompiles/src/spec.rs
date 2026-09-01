use base_common_genesis::BaseUpgrade;
use base_precompile_storage::StorageFeatures;

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
