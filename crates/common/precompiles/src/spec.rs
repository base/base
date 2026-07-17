use base_common_genesis::BaseUpgrade;
use base_precompile_storage::StorageSemantics;

/// Resolves Base upgrades into fork-dependent persistent-storage semantics.
#[derive(Debug, Clone, Copy)]
pub struct BaseStorageSemantics;

impl BaseStorageSemantics {
    /// Returns the persistent-storage semantics active at `upgrade`.
    pub const fn from_upgrade(upgrade: BaseUpgrade) -> StorageSemantics {
        if upgrade as u8 >= BaseUpgrade::Cobalt as u8 {
            StorageSemantics::Cobalt
        } else {
            StorageSemantics::Legacy
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
    fn storage_semantics_activate_at_cobalt() {
        assert_eq!(
            BaseStorageSemantics::from_upgrade(BaseUpgrade::Beryl),
            StorageSemantics::Legacy,
        );
        assert_eq!(
            BaseStorageSemantics::from_upgrade(BaseUpgrade::Cobalt),
            StorageSemantics::Cobalt,
        );
    }
}
