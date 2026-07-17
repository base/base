//! Versioned business logic for the stablecoin B-20 precompile.
//!
//! [`Stablecoin`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20StablecoinToken`] is the minimal
//! storage + policy holder the logic operates on; and [`StablecoinV1`] is the
//! first frozen implementation.

use alloy_primitives::Address;

use crate::{PolicyAccounting, PolicyRegistryLogic, PolicyVersion, StablecoinAccounting, Token};

mod interface;
pub use interface::Stablecoin;

mod v1;
pub use v1::StablecoinV1;

/// Storage + policy binding the stablecoin logic operates on.
///
/// A minimal `(accounting, policy, policy_version)` holder implementing [`Token`];
/// it carries no behavior of its own — all business logic lives in the version
/// implementations resolved from [`crate::StablecoinVersions`]. Authorization goes
/// through [`crate::PolicyRegistryLogic`] via [`Token::policy`].
#[derive(Debug, Clone)]
pub struct B20StablecoinToken<S: StablecoinAccounting, A: PolicyAccounting> {
    accounting: S,
    policy: A,
    policy_version: PolicyVersion,
}

impl<S: StablecoinAccounting, A: PolicyAccounting> B20StablecoinToken<S, A> {
    /// Creates a holder backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        accounting: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { accounting, policy, policy_version }
    }
}

impl<S: StablecoinAccounting, A: PolicyAccounting> Token for B20StablecoinToken<S, A> {
    type Accounting = S;
    type PolicyAccounting = A;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &dyn PolicyRegistryLogic<A> {
        self.policy_version.implementation()
    }

    fn policy_storage(&self) -> &A {
        &self.policy
    }

    fn policy_storage_mut(&mut self) -> &mut A {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}
