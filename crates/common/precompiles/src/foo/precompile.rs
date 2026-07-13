//! Precompile entry point for the `foo` reference precompile.

use alloy_evm::precompiles::PrecompilesMap;
use base_common_genesis::BaseUpgrade;
use base_precompile_macros::precompile;

use crate::{FooLogic, FooStorage, FooVersions};

/// Entry point for the `foo` reference precompile.
///
/// The `#[precompile]` macro generates `precompile(logic)`, which threads the
/// active implementation into [`FooStorage::dispatch`]. Fork→implementation
/// selection is owned here (via [`FooVersions`]); the dispatcher just calls the
/// implementation it is handed. All version routing therefore stays inside the
/// `foo` module.
#[precompile(args(logic: &'static dyn FooLogic))]
#[derive(Debug, Clone, Copy)]
pub struct Foo;

impl Foo {
    /// Installs `foo` for `upgrade`, resolving the active version via
    /// [`FooVersions`]. A no-op before the introduction fork (Beryl), where the
    /// precompile does not exist.
    pub fn install(precompiles: &mut PrecompilesMap, upgrade: BaseUpgrade) {
        let Some(logic) = FooVersions::resolve(upgrade) else {
            return;
        };
        precompiles.extend_precompiles(core::iter::once((
            FooStorage::ADDRESS,
            Self::precompile(logic),
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
