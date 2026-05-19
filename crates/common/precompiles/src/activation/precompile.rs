//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::Bytes;
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::ActivationRegistry;

impl ActivationRegistry {
    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn create_precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("ActivationRegistry".into()), Self::run)
    }

    /// Executes the activation registry precompile.
    pub fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            // Match the shared `base_precompile!` wrapper: invalid call types revert before
            // any work is performed, with no gas charged and no diagnostic revert data.
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }

        let data: Bytes = input.data.to_vec().into();
        let mut storage = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut storage, |ctx| Self::new().dispatch(ctx, &data))
    }
}
