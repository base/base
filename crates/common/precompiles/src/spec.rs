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

    /// Returns the persistent-storage features for a caller that must observe
    /// features from at least `min`, resolving to `from_upgrade(max(BaseUpgrade::LATEST, min))`.
    ///
    /// Intended for callers that lack a live [`BaseUpgrade`] at construction — for
    /// example the enshrined-execution `JournalStorageProvider` in
    /// `crates/common/evm/src/eip8130.rs`, which currently inherits the trait
    /// default `StorageFeatures::Legacy` when accessing `NonceManagerStorage` and
    /// `TxContextStorage` before the first EVM call frame. Regular precompile
    /// wrappers should thread `upgrade` through their constructor and call
    /// [`from_upgrade`](Self::from_upgrade) instead, so their features track the
    /// same signal that gated the wrapper's install.
    pub fn at_least(min: BaseUpgrade) -> StorageFeatures {
        Self::from_upgrade(core::cmp::max(BaseUpgrade::LATEST, min))
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

    #[test]
    fn at_least_honors_floor_when_latest_is_behind() {
        // LATEST is currently pre-Cobalt; a wrapper gated at Cobalt must still get Cobalt features.
        assert!(BaseUpgrade::LATEST < BaseUpgrade::Cobalt);
        assert_eq!(
            UpgradeGatedStorageFeatures::at_least(BaseUpgrade::Cobalt),
            StorageFeatures::Cobalt,
        );
    }

    #[test]
    fn at_least_rides_latest_when_latest_meets_floor() {
        // A wrapper with a pre-Cobalt floor tracks whatever LATEST already provides.
        assert_eq!(
            UpgradeGatedStorageFeatures::at_least(BaseUpgrade::Beryl),
            UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::LATEST),
        );
    }
}
