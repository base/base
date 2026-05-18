//! EVM wiring for the `DefaultToken` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::Bytes;
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::DefaultToken;

/// EVM entry point for the `DefaultToken` precompile.
///
/// Wraps [`DefaultToken`] dispatch behind a [`DynPrecompile`] suitable for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct DefaultTokenEvm;

impl DefaultTokenEvm {
    /// Returns a [`DynPrecompile`] that routes calldata through [`DefaultToken`].
    pub fn precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("DefaultToken".into()), Self::run)
    }

    fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, |ctx| DefaultToken::new(ctx).dispatch(ctx, &calldata))
    }
}
