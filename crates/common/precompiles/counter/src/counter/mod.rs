//! Counter precompile: storage layout, ABI dispatch, and NativePrecompile impl.

use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

pub use dispatch::dispatch;
pub use storage::{COUNTER_ADDRESS, Counter};

mod dispatch;
mod storage;

impl NativePrecompile for Counter {
    const ADDRESS: Address = COUNTER_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        // TODO: wire calldata once PrecompileStorageProvider exposes it
        todo!()
    }
}
