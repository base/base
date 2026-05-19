//! EVM entry point for the `TokenFactory` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::Bytes;
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::storage::TokenFactory;

/// EVM entry point for the `TokenFactory` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenFactoryEvm;

impl TokenFactoryEvm {
    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("TokenFactory".into()), Self::run)
    }

    fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, |ctx| TokenFactory::new(ctx).dispatch(ctx, &calldata))
    }
}
