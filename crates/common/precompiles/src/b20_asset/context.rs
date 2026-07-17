//! Contract context for the asset B-20 precompile.
//!
//! [`ContractContext`] is the minimal storage + policy holder the logic and
//! dispatcher operate on. It carries no business logic of its own — behavior
//! lives in the version implementations resolved from [`crate::VersionResolver`].

use alloy_primitives::Address;

use crate::{AssetAccounting, PolicyAccounting, PolicyLogic, PolicyVersion, Token};

/// Storage + policy binding the asset logic operates on.
///
/// A minimal `(storage, policy, policy_version)` holder implementing [`Token`];
/// it carries no behavior of its own — all business logic lives in the version
/// implementations resolved from [`crate::VersionResolver`]. Authorization goes
/// through [`crate::PolicyLogic`] via [`Token::policy`].
#[derive(Debug, Clone)]
pub struct ContractContext<S: AssetAccounting, A: PolicyAccounting> {
    storage: S,
    policy: A,
    policy_version: PolicyVersion,
}

impl<S: AssetAccounting, A: PolicyAccounting> ContractContext<S, A> {
    /// Creates a context backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        storage: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { storage, policy, policy_version }
    }
}

impl<S: AssetAccounting, A: PolicyAccounting> Token for ContractContext<S, A> {
    type Accounting = S;
    type PolicyAccounting = A;

    fn accounting(&self) -> &S {
        &self.storage
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    fn policy(&self) -> &dyn PolicyLogic<A> {
        self.policy_version.implementation()
    }

    fn policy_storage(&self) -> &A {
        &self.policy
    }

    fn policy_storage_mut(&mut self) -> &mut A {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.storage.token_address()
    }
}
