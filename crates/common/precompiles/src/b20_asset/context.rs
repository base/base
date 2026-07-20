//! Contract context for the asset B-20 precompile.
//!
//! [`AssetContractContext`] is the minimal storage + policy holder the logic and
//! dispatcher operate on. It carries no business logic of its own — behavior
//! lives in the version implementations resolved from [`super::AssetVersionResolver`].

use alloy_primitives::Address;

use crate::{
    AssetAccounting, PolicyAccounting, PolicyContractContext, PolicyRegistryLogic, PolicyVersion,
    Token,
};

/// Storage + policy binding the asset logic operates on.
///
/// A minimal `(storage, policy, policy_version)` holder implementing [`Token`];
/// it carries no behavior of its own — all business logic lives in the version
/// implementations resolved from [`super::AssetVersionResolver`]. Authorization goes
/// through [`crate::PolicyRegistryLogic`] via [`Token::policy`].
#[derive(Debug, Clone)]
pub struct AssetContractContext<S: AssetAccounting, A: PolicyAccounting> {
    storage: S,
    policy: PolicyContractContext<A>,
    policy_version: PolicyVersion,
}

impl<S: AssetAccounting, A: PolicyAccounting> AssetContractContext<S, A> {
    /// Creates a context backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        storage: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { storage, policy: PolicyContractContext::with_storage(policy), policy_version }
    }
}

impl<S: AssetAccounting, A: PolicyAccounting> Token for AssetContractContext<S, A> {
    type Accounting = S;
    type PolicyAccounting = A;

    fn accounting(&self) -> &S {
        &self.storage
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    fn policy(&self) -> &dyn PolicyRegistryLogic<A> {
        self.policy_version.implementation()
    }

    fn policy_context(&self) -> &PolicyContractContext<A> {
        &self.policy
    }

    fn policy_context_mut(&mut self) -> &mut PolicyContractContext<A> {
        &mut self.policy
    }

    fn policy_storage(&self) -> &A {
        self.policy.storage()
    }

    fn policy_storage_mut(&mut self) -> &mut A {
        self.policy.storage_mut()
    }

    fn token_address(&self) -> Address {
        self.storage.token_address()
    }
}
