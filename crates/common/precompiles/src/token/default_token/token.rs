//! `DefaultToken` struct — the concrete B-20 token type.

use alloy_primitives::Address;
use base_precompile_storage::StorageCtx;

use super::storage::{DEFAULT_TOKEN_ADDRESS, DefaultTokenStorage};
use crate::token::common::{
    Burnable, Configurable, Mintable, NoOpPolicyRegistry, Pausable, Permittable, PolicyRegistry,
    Redeemable, Token, TokenAccounting, Transferable,
};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. The
/// generic `P` lets callers inject a [`PolicyRegistry`] implementation;
/// it defaults to [`NoOpPolicyRegistry`] so existing callers need no changes.
/// In production, [`DefaultToken::new`] wires in [`DefaultTokenStorage`].
#[derive(Debug, Clone)]
pub struct DefaultToken<S: TokenAccounting, P: PolicyRegistry = NoOpPolicyRegistry> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<'a> DefaultToken<DefaultTokenStorage<'a>> {
    /// Creates a new `DefaultToken` backed by [`DefaultTokenStorage`].
    pub fn new(storage: StorageCtx<'a>) -> Self {
        Self { accounting: DefaultTokenStorage::new(storage), policy: NoOpPolicyRegistry }
    }
}

impl<S: TokenAccounting> DefaultToken<S> {
    /// Creates a `DefaultToken` backed by the provided storage adapter.
    ///
    /// Use this in tests to inject an in-memory [`TokenAccounting`] implementation.
    pub const fn with_storage(accounting: S) -> Self {
        Self { accounting, policy: NoOpPolicyRegistry }
    }
}

impl<S: TokenAccounting, P: PolicyRegistry> DefaultToken<S, P> {
    /// Creates a `DefaultToken` backed by the provided storage and policy adapters.
    pub fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting and policy fields, fix the precompile address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: PolicyRegistry> Token for DefaultToken<S, P> {
    type Accounting = S;
    type Policy = P;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &P {
        &self.policy
    }

    fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        DEFAULT_TOKEN_ADDRESS
    }
}

// ---------------------------------------------------------------------------
// Capability selection — DefaultToken opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: PolicyRegistry> Transferable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Mintable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Burnable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Redeemable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Pausable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Configurable for DefaultToken<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Permittable for DefaultToken<S, P> {}
