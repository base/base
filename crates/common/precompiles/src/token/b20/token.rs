//! `B20Token` struct — the concrete B-20 token type.

use alloy_primitives::Address;
use base_precompile_storage::StorageCtx;

use super::storage::{B20_TOKEN_ADDRESS, B20TokenStorage};
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
/// In production, [`B20Token::new`] wires in [`B20TokenStorage`].
#[derive(Debug, Clone)]
pub struct B20Token<S: TokenAccounting, P: PolicyRegistry = NoOpPolicyRegistry> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<'a> B20Token<B20TokenStorage<'a>> {
    /// Creates a new `B20Token` backed by [`B20TokenStorage`].
    pub fn new(storage: StorageCtx<'a>) -> Self {
        Self { accounting: B20TokenStorage::new(storage), policy: NoOpPolicyRegistry }
    }
}

impl<S: TokenAccounting> B20Token<S> {
    /// Creates a `B20Token` backed by the provided storage adapter.
    ///
    /// Use this in tests to inject an in-memory [`TokenAccounting`] implementation.
    pub const fn with_storage(accounting: S) -> Self {
        Self { accounting, policy: NoOpPolicyRegistry }
    }
}

impl<S: TokenAccounting, P: PolicyRegistry> B20Token<S, P> {
    /// Creates a `B20Token` backed by the provided storage and policy adapters.
    pub fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting and policy fields, fix the precompile address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: PolicyRegistry> Token for B20Token<S, P> {
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
        B20_TOKEN_ADDRESS
    }
}

// ---------------------------------------------------------------------------
// Capability selection — B20Token opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: PolicyRegistry> Transferable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Mintable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Burnable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Redeemable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Pausable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Configurable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyRegistry> Permittable for B20Token<S, P> {}
