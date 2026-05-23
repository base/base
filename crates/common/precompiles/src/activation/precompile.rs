//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;

use super::ActivationRegistryStorage;
use crate::macros::base_precompile;

/// Entry point for the activation registry precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Installs the singleton activation registry precompile into `precompiles`.
    pub fn install(precompiles: &mut PrecompilesMap, activation_admin_address: Option<Address>) {
        precompiles.extend_precompiles(core::iter::once((
            ActivationRegistryStorage::ADDRESS,
            Self::precompile(activation_admin_address),
        )));
    }

    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn precompile(activation_admin_address: Option<Address>) -> DynPrecompile {
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            ActivationRegistryStorage::new(ctx).dispatch(ctx, &calldata, activation_admin_address)
        })
    }
}
