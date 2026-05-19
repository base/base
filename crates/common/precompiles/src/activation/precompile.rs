//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::DynPrecompile;

use super::ActivationRegistry;
use crate::macros::base_precompile;

impl ActivationRegistry {
    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn create_precompile() -> DynPrecompile {
        base_precompile!("ActivationRegistry", |ctx, calldata| {
            Self::new().dispatch(ctx, &calldata)
        })
    }
}
