//! DefaultToken native precompile — the base B-20 variant.

use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

pub use dispatch::dispatch;
pub use storage::{DEFAULT_TOKEN_ADDRESS, DefaultToken};

mod dispatch;
mod storage;

impl NativePrecompile for DefaultToken {
    const ADDRESS: Address = DEFAULT_TOKEN_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        todo!("wire calldata once PrecompileStorageProvider exposes it")
    }
}
