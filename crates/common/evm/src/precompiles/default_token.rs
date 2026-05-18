//! EVM entry point for the DefaultToken native precompile.

use alloy_primitives::{Address, Bytes, address};
use base_common_precompiles::{DefaultToken, dispatch};
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileOutput, PrecompileResult};

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};

/// Canonical address of the DefaultToken precompile.
pub const ADDRESS: Address = base_common_precompiles::DEFAULT_TOKEN_ADDRESS;

/// EVM entry point for the DefaultToken precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultTokenPrecompile;

impl DefaultTokenPrecompile {
    /// Returns a [`DynPrecompile`] registerable with a [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(
            revm::precompile::PrecompileId::Custom("DefaultToken".into()),
            Self::run,
        )
    }

    fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, || {
            let mut pc = DefaultToken::new();
            dispatch(&mut pc, &calldata)
        })
    }
}
