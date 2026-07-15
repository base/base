//! Versioned business logic for the `PolicyRegistry` precompile.
//!
//! [`PolicyRegistryLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`PolicyRegistryRuntime`] is the minimal storage
//! binding the logic operates on; and [`PolicyRegistryV1`] is the first frozen
//! implementation.

use crate::PolicyAccounting;

mod interface;
pub use interface::PolicyRegistryLogic;

mod v1;
pub use v1::PolicyRegistryV1;

/// Storage binding the policy-registry logic operates on.
///
/// A minimal holder over a single storage adapter `S`; it carries no behavior of its own
/// — all business logic lives in the version implementations resolved from
/// [`crate::PolicyVersions`]. Unlike the B-20 token holders it does not bind a separate
/// policy port (the registry *is* the policy source) and does not implement the token
/// [`crate::Token`] trait. It is parameterized by the storage adapter (`S`) so tests can
/// inject an in-memory backend while production uses the EVM-backed storage.
#[derive(Debug, Clone)]
pub struct PolicyRegistryRuntime<S: PolicyAccounting> {
    accounting: S,
}

impl<S: PolicyAccounting> PolicyRegistryRuntime<S> {
    /// Creates a runtime backed by the provided storage adapter.
    pub const fn with_storage(accounting: S) -> Self {
        Self { accounting }
    }

    /// Returns a shared reference to the storage adapter.
    pub const fn accounting(&self) -> &S {
        &self.accounting
    }

    /// Returns a mutable reference to the storage adapter.
    pub const fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }
}
