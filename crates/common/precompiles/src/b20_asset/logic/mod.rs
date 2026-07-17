//! Versioned business logic for the asset B-20 precompile.
//!
//! [`Asset`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20AssetToken`] is the minimal
//! storage + policy holder the logic operates on; and [`AssetV1`] is the
//! first frozen implementation.

use alloy_primitives::Address;

use crate::{AssetAccounting, PolicyAccounting, PolicyRegistryLogic, PolicyVersion, Token};

mod interface;
pub use interface::Asset;

mod v1;
pub use v1::AssetV1;

/// Storage + policy binding the asset logic operates on.
///
/// A minimal `(accounting, policy, policy_version)` holder implementing [`Token`];
/// it carries no behavior of its own — all business logic lives in the version
/// implementations resolved from [`crate::AssetVersions`]. Authorization goes
/// through [`crate::PolicyRegistryLogic`] via [`Token::policy`].
#[derive(Debug, Clone)]
pub struct B20AssetToken<S: AssetAccounting, A: PolicyAccounting> {
    accounting: S,
    policy: A,
    policy_version: PolicyVersion,
}

impl<S: AssetAccounting, A: PolicyAccounting> B20AssetToken<S, A> {
    /// Creates a holder backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        accounting: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { accounting, policy, policy_version }
    }
}

impl<S: AssetAccounting, A: PolicyAccounting> Token for B20AssetToken<S, A> {
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
