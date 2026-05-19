//! `B20Token` struct — the concrete B-20 token type.

use alloy_primitives::Address;
use base_precompile_storage::StorageCtx;

use super::storage::B20TokenStorage;
use crate::token::common::{
    Burnable, Configurable, Mintable, Pausable, Permittable, Redeemable, Token, TokenAccounting,
    Transferable,
};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. In
/// production, the storage adapter is bound to the address selected by the
/// dynamic precompile lookup.
#[derive(Debug, Clone)]
pub struct B20Token<S: TokenAccounting> {
    pub(super) accounting: S,
}

impl<'a> B20Token<B20TokenStorage<'a>> {
    /// Creates a new `B20Token` backed by [`B20TokenStorage`].
    pub fn new(storage: StorageCtx<'a>) -> Self {
        Self { accounting: B20TokenStorage::new(storage) }
    }
}

impl<S: TokenAccounting> B20Token<S> {
    /// Creates a `B20Token` backed by the provided storage adapter.
    ///
    /// Use this in tests to inject an in-memory [`TokenAccounting`] implementation.
    pub const fn with_storage(accounting: S) -> Self {
        Self { accounting }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting field and dynamic token address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting> Token for B20Token<S> {
    type Accounting = S;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}

// ---------------------------------------------------------------------------
// Capability selection — B20Token opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: TokenAccounting> Transferable for B20Token<S> {}
impl<S: TokenAccounting> Mintable for B20Token<S> {}
impl<S: TokenAccounting> Burnable for B20Token<S> {}
impl<S: TokenAccounting> Redeemable for B20Token<S> {}
impl<S: TokenAccounting> Pausable for B20Token<S> {}
impl<S: TokenAccounting> Configurable for B20Token<S> {}
impl<S: TokenAccounting> Permittable for B20Token<S> {}
