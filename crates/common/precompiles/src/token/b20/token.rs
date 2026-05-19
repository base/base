//! `B20Token` struct — the concrete B-20 token type.

use alloy_primitives::Address;

use super::storage::B20_TOKEN_ADDRESS;
use crate::{Policy, token::{
    PolicyStorage,
    common::{
        Burnable, Configurable, Mintable, Pausable, Permittable, Redeemable, Token,
        TokenAccounting, Transferable,
    },
}};

/// EVM precompile for the Default B-20 token variant.
///
/// The generic `S` lets callers swap in an in-memory [`TokenAccounting`]
/// implementation for unit tests without touching real EVM storage. The
/// generic `P` provides the [`PolicyStorage`] implementation consulted on
/// every transfer and mint. In production, [`B20Token::with_storage_and_policy`]
/// wires in [`B20TokenStorage`] and [`Policy`].
#[derive(Debug, Clone)]
pub struct B20Token<S: TokenAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<S: TokenAccounting, P: PolicyStorage> B20Token<S, P> {
    /// Creates a `B20Token` backed by the provided storage and policy adapters.
    pub fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

// ---------------------------------------------------------------------------
// Token: wire the accounting and policy fields, fix the precompile address
// ---------------------------------------------------------------------------

impl<S: TokenAccounting, P: PolicyStorage> Token for B20Token<S, P> {
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

impl<S: TokenAccounting, P: PolicyStorage> Transferable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Mintable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Burnable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Redeemable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Pausable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Configurable for B20Token<S, P> {}
impl<S: TokenAccounting, P: PolicyStorage> Permittable for B20Token<S, P> {}
