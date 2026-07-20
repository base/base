//! Contract context for the `PolicyRegistry` precompile.
//!
//! [`ContractContext`] is the minimal storage holder the logic and dispatcher
//! operate on. It carries no business logic of its own — behavior lives in the
//! version implementations resolved from [`super::VersionResolver`].

use super::PolicyAccounting;

/// Storage binding the policy-registry logic operates on.
///
/// A minimal `storage` holder; it carries no behavior of its own — all business
/// logic lives in the version implementations resolved from
/// [`super::VersionResolver`].
#[derive(Debug, Clone)]
pub struct ContractContext<S: PolicyAccounting> {
    storage: S,
}

impl<S: PolicyAccounting> ContractContext<S> {
    /// Creates a context backed by policy-registry storage.
    pub const fn with_storage(storage: S) -> Self {
        Self { storage }
    }

    /// Returns a shared reference to the underlying storage port.
    pub const fn storage(&self) -> &S {
        &self.storage
    }

    /// Returns an exclusive reference to the underlying storage port.
    pub const fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }
}
