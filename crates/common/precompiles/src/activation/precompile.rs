//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
use base_precompile_macros::precompile;

use crate::{ActivationRegistryStorage, PrecompileCallObserver, macros::base_precompile};

/// Entry point for the activation registry precompile.
#[precompile(install, args(activation_admin_address: Option<Address>))]
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Installs the activation registry precompile with an observer.
    pub fn install_with_observer<O>(
        precompiles: &mut PrecompilesMap,
        activation_admin_address: Option<Address>,
        observer: O,
    ) where
        O: PrecompileCallObserver,
    {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile_with_observer(activation_admin_address, observer),
        )));
    }

    /// Creates the EVM precompile wrapper for the activation registry with an observer.
    pub fn precompile_with_observer<O>(
        activation_admin_address: Option<Address>,
        observer: O,
    ) -> DynPrecompile
    where
        O: PrecompileCallObserver,
    {
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            let observer = observer.clone();
            ActivationRegistryStorage::new(ctx).dispatch_with_observer(
                ctx,
                &calldata,
                activation_admin_address,
                observer,
            )
        })
    }
}
