//! EVM wiring for the `DefaultToken` precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, Bytes};
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::{DefaultToken, storage::DefaultTokenStorage};

/// EVM entry point for the `DefaultToken` precompile.
///
/// Wraps [`DefaultToken`] dispatch behind a [`DynPrecompile`] suitable for
/// registration in a [`PrecompilesMap`].
#[derive(Debug)]
pub struct DefaultTokenEvm;

impl DefaultTokenEvm {
    /// Returns a [`DynPrecompile`] that dispatches to the [`DefaultToken`] logic at `token_address`.
    ///
    /// Used by the precompile-lookup fallback to route calls to any B-20 token address.
    pub fn create_precompile(token_address: Address) -> DynPrecompile {
        DynPrecompile::new_stateful(
            PrecompileId::Custom(alloc::format!("DefaultToken@{token_address}").into()),
            move |input| Self::run_at(input, token_address),
        )
    }

    fn run_at(input: PrecompileInput<'_>, token_address: Address) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, |ctx| {
            DefaultToken::with_storage(DefaultTokenStorage::from_address(token_address, ctx))
                .dispatch(ctx, &calldata)
        })
    }
}
