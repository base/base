//! DefaultToken native precompile — the base B-20 token variant.

use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

mod dispatch;
mod evm;
mod storage;
pub use evm::DefaultTokenEvm;
pub use storage::{DEFAULT_TOKEN_ADDRESS, DefaultTokenStorage};

use crate::token::common::TokenBase;

/// EVM precompile for the Default B-20 token variant.
#[derive(Debug)]
pub struct DefaultToken {
    /// Shared domain logic wrapping the storage adapter.
    pub base: TokenBase<DefaultTokenStorage>,
}

impl DefaultToken {
    /// Creates a new `DefaultToken` bound to [`DEFAULT_TOKEN_ADDRESS`].
    pub fn new() -> Self {
        Self { base: TokenBase::new(DefaultTokenStorage::new(), DEFAULT_TOKEN_ADDRESS) }
    }
}

impl Default for DefaultToken {
    fn default() -> Self {
        Self::new()
    }
}

impl NativePrecompile for DefaultToken {
    const ADDRESS: Address = DEFAULT_TOKEN_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        todo!("wire calldata once PrecompileStorageProvider exposes it")
    }
}
