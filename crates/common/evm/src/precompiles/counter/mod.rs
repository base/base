//! EVM entry point for the Counter native precompile.
//!
//! [`CounterPrecompile`] bridges the domain logic in `base-precompile-counter` to the live EVM
//! via [`EvmPrecompileStorageProvider`] and [`StorageCtx::enter`]. It is registered fork-gated
//! in [`crate::factory::BaseEvmFactory::precompiles`] at [`BaseUpgrade::Beryl`].

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, Bytes, address};
use base_precompile_counter::{Counter, dispatch};
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::{
    precompile::{PrecompileId, PrecompileOutput, PrecompileResult},
    state::Bytecode,
};

/// Canonical address of the Counter precompile.
pub const ADDRESS: Address = address!("0000000000000000000000000000000000000900");

/// EF bytecode sentinel that marks a precompile address as non-empty.
///
/// Without deployed code, the account at `ADDRESS` is an EIP-161 empty account and
/// Ethereum clears it (including any storage) at end-of-transaction. Setting this
/// single-byte sentinel prevents that cleanup, exactly as `#[contract]`'s generated
/// `__initialize()` does.
const SENTINEL: &[u8] = &[0xef];

/// EVM entry point for the Counter precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct CounterPrecompile;

impl CounterPrecompile {
    /// Returns a [`DynPrecompile`] that can be registered with [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("Counter".into()), Self::run)
    }

    fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        // Capture calldata before consuming input.
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, || {
            let mut ctx = StorageCtx;
            // Ensure the account at ADDRESS has code so EIP-161 doesn't purge its
            // storage at end-of-transaction.  Idempotent: set_code is a no-op when
            // the code is already the sentinel.
            let _ = ctx.set_code(ADDRESS, Bytecode::new_legacy(Bytes::from_static(SENTINEL)));

            let mut counter = Counter::new();
            dispatch(&mut counter, &calldata)
        })
    }
}
