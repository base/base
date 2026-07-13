//! Precompile entry point for the `foo` reference precompile.

use base_precompile_macros::precompile;

use crate::{FooStorage, FooVersion};

/// Entry point for the `foo` reference precompile.
///
/// The `#[precompile]` macro generates `install(precompiles, version)` and
/// `precompile(version)`, threading the resolved [`FooVersion`] into
/// [`FooStorage::dispatch`].
#[precompile(args(version: FooVersion), install)]
#[derive(Debug, Clone, Copy)]
pub struct Foo;

#[cfg(test)]
mod tests {
    use alloy_evm::precompiles::PrecompilesMap;
    use revm::precompile::Precompiles;

    use crate::{Foo, FooStorage, FooVersion};

    #[test]
    fn install_registers_precompile_address() {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::cancun());

        Foo::install(&mut precompiles, FooVersion::V1);

        assert!(precompiles.get(&FooStorage::ADDRESS).is_some());
    }
}
