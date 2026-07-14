//! Versioned business logic for the stablecoin B-20 precompile.
//!
//! [`Stablecoin`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20StablecoinToken`] is the minimal
//! storage + policy holder the logic operates on; and [`StablecoinV1`] is the
//! first frozen implementation.

use alloy_primitives::Address;

use crate::{Policy, StablecoinAccounting, Token};

mod interface;
pub use interface::Stablecoin;

mod v1;
pub use v1::StablecoinV1;

/// Storage + policy binding the stablecoin logic operates on.
///
/// A minimal `(accounting, policy)` holder implementing [`Token`]; it carries no
/// behavior of its own — all business logic lives in the version implementations
/// resolved from [`crate::StablecoinVersions`]. It is parameterized by the
/// storage (`S`) and policy (`P`) adapters so tests can inject in-memory
/// backends while production uses the EVM-backed storage.
#[derive(Debug, Clone)]
pub struct B20StablecoinToken<S: StablecoinAccounting, P: Policy> {
    accounting: S,
    policy: P,
}

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// Creates a holder backed by the provided storage and policy adapters.
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
