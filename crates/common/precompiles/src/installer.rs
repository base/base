use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
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
            precompiles.set_precompile_lookup(b20_lookup);
        }
    }
}

// Function pointer (not a closure) satisfies the HRTB `for<'a> Fn(&'a Address) -> Option<DynPrecompile>`
// required by `set_precompile_lookup`.
fn b20_lookup(address: &Address) -> Option<DynPrecompile> {
    if *address == crate::token::FACTORY_ADDRESS {
        Some(crate::token::TokenFactoryPrecompile::precompile())
    } else {
        match crate::token::variant_of(address) {
            crate::token::VARIANT_DEFAULT => {
                Some(crate::token::B20TokenPrecompile::create_precompile(*address))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use revm::precompile::{bn254, secp256r1};
    use rstest::rstest;

    use super::*;
    use crate::token::{FACTORY_ADDRESS, VARIANT_DEFAULT, compute_b20_address};

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

    #[rstest]
    #[case::azul(BaseUpgrade::Azul, false)]
    #[case::beryl(BaseUpgrade::Beryl, true)]
    fn installer_routes_b20_precompiles_by_fork(#[case] spec: BaseUpgrade, #[case] expected: bool) {
        let precompiles = BasePrecompileInstaller::new(spec).install();
        let (token, _) = compute_b20_address(
            Address::repeat_byte(0x11),
            VARIANT_DEFAULT,
            18,
            B256::repeat_byte(0x22),
        );

        assert_eq!(precompiles.get(&FACTORY_ADDRESS).is_some(), expected);
        assert_eq!(precompiles.get(&token).is_some(), expected);
        assert!(precompiles.get(&Address::repeat_byte(0x42)).is_none());
    }
}
