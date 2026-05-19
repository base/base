//! Precompile entry point for the activation registry.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_sol_types::SolError as _;
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::{ActivationRegistry, IActivationRegistry};

impl ActivationRegistry {
    /// Creates the EVM precompile wrapper for the activation registry.
    pub fn create_precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("ActivationRegistry".into()), Self::run)
    }

    /// Executes the activation registry precompile.
    pub fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            // No gas charged: the call type is invalid before any work is performed.
            return Ok(PrecompileOutput::new_reverted(
                0,
                IActivationRegistry::DelegateCallNotAllowed {}.abi_encode().into(),
            ));
        }

        let data = input.data;
        let mut storage = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut storage, |ctx| Self::new().dispatch(ctx, data))
    }
}
