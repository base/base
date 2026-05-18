use alloy_evm::precompiles::PrecompilesMap;
use base_common_chains::BaseUpgrade;

use crate::{BasePrecompileSpec, BasePrecompiles};

/// Installs the full Base precompile set for a given spec.
#[derive(Debug, Clone, Copy)]
pub struct BasePrecompileInstaller<S = BaseUpgrade> {
    /// Spec used to select the Base precompile set.
    spec: S,
}

impl<S: BasePrecompileSpec> BasePrecompileInstaller<S> {
    /// Creates a new installer for the given spec.
    pub const fn new(spec: S) -> Self {
        Self { spec }
    }

    /// Returns the spec used by this installer.
    pub const fn spec(&self) -> S {
        self.spec
    }

    /// Builds a [`PrecompilesMap`] with all Base precompiles installed.
    pub fn install(self) -> PrecompilesMap {
        let mut precompiles =
            PrecompilesMap::from_static(BasePrecompiles::new_with_spec(self.spec).precompiles());
        self.install_into(&mut precompiles);
        precompiles
    }

    /// Installs Base-specific dynamic precompiles into an existing [`PrecompilesMap`].
    pub fn install_into(self, precompiles: &mut PrecompilesMap) {
        if self.spec.upgrade() >= BaseUpgrade::Beryl {
            precompiles.apply_precompile(&crate::token::DEFAULT_TOKEN_ADDRESS, |_| {
                Some(crate::token::DefaultTokenEvm::precompile())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use revm::precompile::{bn254, secp256r1};

    use super::*;

    #[test]
    fn installer_preserves_base_precompile_set() {
        let precompiles = BasePrecompileInstaller::new(BaseUpgrade::Jovian).install();

        assert!(precompiles.get(&bn254::pair::ADDRESS).is_some());
        assert!(precompiles.get(secp256r1::P256VERIFY.address()).is_some());
    }

    #[test]
    fn default_installer_uses_default_precompile_spec() {
        let installer = BasePrecompileInstaller::new(BaseUpgrade::LATEST);

        assert_eq!(installer.spec(), BaseUpgrade::LATEST);
    }
}
