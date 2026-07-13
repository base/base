//! Precompile entry point for the `foo` reference precompile.

use alloy_evm::precompiles::PrecompilesMap;
use base_common_genesis::BaseUpgrade;
use base_precompile_macros::precompile;

use crate::{FooStorage, FooVersion, FooVersions};

/// Entry point for the `foo` reference precompile.
///
/// The `#[precompile]` macro generates `precompile(version)`, which threads the
/// active implementation (`&dyn FooVersion`) into [`FooStorage::dispatch`].
/// Fork→version selection is owned by [`FooVersions`]; the version does its own
/// decoding and routing. All version logic therefore stays inside the `foo`
/// module.
#[precompile(args(version: &'static dyn FooVersion))]
#[derive(Debug, Clone, Copy)]
pub struct Foo;

impl Foo {
    /// Installs `foo` for `upgrade`, resolving the active version via
    /// [`FooVersions`]. A no-op before the introduction fork (Beryl), where the
    /// precompile does not exist.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        let Some(version) = FooVersions::resolve(upgrade) else {
            return;
        };
        precompiles.extend_precompiles(core::iter::once((
            FooStorage::ADDRESS,
            Self::precompile(version),
        )));
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use base_common_genesis::BaseUpgrade;
    use revm::precompile::Precompiles;

    use crate::{Foo, FooStorage};

    #[test]
    fn install_registers_precompile_from_beryl() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        Foo::install(&mut precompiles, BaseUpgrade::Beryl);

        assert!(precompiles.get(&FooStorage::ADDRESS).is_some());
    }

    #[test]
    fn install_is_noop_before_beryl() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        Foo::install(&mut precompiles, BaseUpgrade::Azul);

        assert!(precompiles.get(&FooStorage::ADDRESS).is_none());
    }
}
