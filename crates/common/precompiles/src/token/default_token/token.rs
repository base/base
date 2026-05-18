//! `DefaultToken` struct — the concrete B-20 token type.

use alloy_primitives::Address;

use super::storage::{DEFAULT_TOKEN_ADDRESS, DefaultTokenStorage};
use crate::token::common::{
    Token, TokenAccounting,
    ops::{Burnable, Configurable, Mintable, Pausable, Permittable, Redeemable, Transferable},
};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. In
/// production `S` defaults to [`DefaultTokenStorage`].
#[derive(Debug, Clone)]
pub struct DefaultToken<S: TokenAccounting = DefaultTokenStorage> {
    pub(super) accounting: S,
}

impl DefaultToken {
    /// Creates a new `DefaultToken` backed by [`DefaultTokenStorage`].
    pub fn new() -> Self {
        Self { accounting: DefaultTokenStorage::new() }
    }
}

impl<S: TokenAccounting> DefaultToken<S> {
    /// Creates a `DefaultToken` backed by the provided storage adapter.
    ///
    /// Use this in tests to inject an in-memory [`TokenAccounting`] implementation.
    pub const fn with_storage(accounting: S) -> Self {
        Self { accounting }
    }
}

impl Default for DefaultToken {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting field and fix the precompile address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting> Token for DefaultToken<S> {
    type Accounting = S;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn token_address(&self) -> Address {
        DEFAULT_TOKEN_ADDRESS
    }
}

// ---------------------------------------------------------------------------
// Capability selection — DefaultToken opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: TokenAccounting> Transferable for DefaultToken<S> {}
impl<S: TokenAccounting> Mintable for DefaultToken<S> {}
impl<S: TokenAccounting> Burnable for DefaultToken<S> {}
impl<S: TokenAccounting> Redeemable for DefaultToken<S> {}
impl<S: TokenAccounting> Pausable for DefaultToken<S> {}
impl<S: TokenAccounting> Configurable for DefaultToken<S> {}
impl<S: TokenAccounting> Permittable for DefaultToken<S> {}
