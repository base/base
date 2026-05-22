//! `B20Token` struct — the concrete B-20 token type.

use alloy_primitives::Address;

use crate::{
    Burnable, Configurable, Mintable, Pausable, Permittable, Policy, RoleManaged, Token,
    TokenAccounting, Transferable,
};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. The
/// generic `P` provides the [`Policy`] implementation consulted for policy
/// decisions. In production, the dynamic precompile lookup wires storage and
/// policy adapters from the same EVM context.
#[derive(Debug, Clone)]
pub struct B20Token<S: TokenAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<S: TokenAccounting, P: Policy> B20Token<S, P> {
    /// Creates a `B20Token` backed by the provided storage and policy adapters.
    ///
    /// Use this in tests to inject in-memory [`TokenAccounting`] and [`Policy`] implementations.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting field and dynamic token address
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
impl<S: TokenAccounting, P: Policy> Pausable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Configurable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> Permittable for B20Token<S, P> {}
impl<S: TokenAccounting, P: Policy> RoleManaged for B20Token<S, P> {}
