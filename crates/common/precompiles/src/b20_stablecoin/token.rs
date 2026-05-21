//! `B20StablecoinToken` struct — the stablecoin B-20 token type.

use alloy_primitives::Address;

use crate::{
    Burnable, Configurable, Mintable, Pausable, Permittable, Policy, Redeemable, Token,
    Transferable,
};
use super::accounting::StablecoinAccounting;

/// EVM precompile for the stablecoin B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S:
/// [`StablecoinAccounting`] so the dispatch layer can read `currency()` from
/// storage. All inherited `IB20` capability traits are wired in identically.
#[derive(Debug, Clone)]
pub struct B20StablecoinToken<S: StablecoinAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// Creates a `B20StablecoinToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

impl<S: StablecoinAccounting, P: Policy> Token for B20StablecoinToken<S, P> {
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

impl<S: StablecoinAccounting, P: Policy> Transferable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Mintable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Burnable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Redeemable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Pausable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Configurable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Permittable for B20StablecoinToken<S, P> {}
