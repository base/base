//! `B20Token` struct — the concrete B-20 token type.

use alloy_primitives::Address;

use crate::{
    Burnable, Configurable, Mintable, Pausable, Permittable, Policy, Redeemable, Token,
    TokenAccounting, Transferable,
};

/// EVM precompile for the B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. The
/// generic `P` provides the [`Policy`] implementation consulted on
/// every transfer and mint. In production,
/// [`B20Token::with_storage_and_policy`] wires in [`crate::B20TokenStorage`]
/// and [`Policy`].
#[derive(Debug, Clone)]
pub struct B20Token<S: TokenAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<S: TokenAccounting, P: Policy> B20Token<S, P> {
    /// Creates a `B20Token` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting and policy fields, dynamic token address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: Policy> Token for B20Token<S, P> {
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
        self.accounting.token_address()
    }
}

// ---------------------------------------------------------------------------
// Capability selection — B20Token opts in to all capabilities
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: Policy> Transferable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Mintable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Burnable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Redeemable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Pausable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Configurable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Permittable for B20Token<S, P> {}
