//! DefaultToken native precompile — the base B-20 token variant.

use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

mod dispatch;
mod evm;
mod storage;
pub use evm::DefaultTokenEvm;
pub use storage::{DEFAULT_TOKEN_ADDRESS, DefaultTokenStorage};

use crate::token::common::{
    IToken, ITokenCoreAccounting,
    ops::{Burnable, Mintable, Pausable, Permittable, Redeemable, TokenAdmin, Transferable},
};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`ITokenCoreAccounting`]
/// implementation for unit tests without touching real EVM storage. In
/// production the default resolves to [`DefaultTokenStorage`].
#[derive(Debug, Clone)]
pub struct DefaultToken<S: ITokenCoreAccounting = DefaultTokenStorage> {
    accounting: S,
}

impl DefaultToken {
    /// Creates a new `DefaultToken` backed by [`DefaultTokenStorage`].
    pub fn new() -> Self {
        Self { accounting: DefaultTokenStorage::new() }
    }
}

impl<S: ITokenCoreAccounting> DefaultToken<S> {
    /// Creates a `DefaultToken` backed by the provided storage adapter.
    ///
    /// Use this in tests to inject an in-memory [`ITokenCoreAccounting`].
    pub fn with_storage(accounting: S) -> Self {
        Self { accounting }
    }
}

impl Default for DefaultToken {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// IToken: wire the accounting field and fix the precompile address
// ---------------------------------------------------------------------------

impl<S: ITokenCoreAccounting> IToken for DefaultToken<S> {
    fn accounting(&self) -> &dyn ITokenCoreAccounting {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut dyn ITokenCoreAccounting {
        &mut self.accounting
    }

    fn token_address(&self) -> Address {
        DEFAULT_TOKEN_ADDRESS
    }
}

// ---------------------------------------------------------------------------
// Capability selection — DefaultToken opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: ITokenCoreAccounting> Transferable for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> Mintable     for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> Burnable     for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> Redeemable   for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> Pausable     for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> TokenAdmin   for DefaultToken<S> {}
impl<S: ITokenCoreAccounting> Permittable  for DefaultToken<S> {}

// ---------------------------------------------------------------------------
// EVM wiring
// ---------------------------------------------------------------------------

impl<S: ITokenCoreAccounting> NativePrecompile for DefaultToken<S> {
    const ADDRESS: Address = DEFAULT_TOKEN_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        todo!("wire calldata once PrecompileStorageProvider exposes it")
    }
}
