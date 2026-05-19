//! Business logic for the `Policy` precompile.
//!
//! `PolicyHandle` is the concrete type the token holds. It wraps [`PolicyRegistryStorage`]
//! and implements the [`Policy`] trait, separating the authorization
//! decisions (here) from the raw storage reads (`storage.rs`).

use core::fmt;

use alloy_primitives::Address;
use base_precompile_storage::{Result, StorageCtx};

use super::storage::PolicyRegistryStorage;
use crate::token::common::Policy;

/// Wraps [`PolicyRegistryStorage`] and implements the [`Policy`] trait,
/// separating authorization decisions from raw storage reads.
pub struct PolicyHandle<'a> {
    inner: PolicyRegistryStorage<'a>,
}

impl<'a> PolicyHandle<'a> {
    /// Creates a `PolicyHandle` backed by the registry storage at its singleton address.
    pub fn new(ctx: StorageCtx<'a>) -> Self {
        Self { inner: PolicyRegistryStorage::new(ctx) }
    }
}

impl fmt::Debug for PolicyHandle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyHandle").finish_non_exhaustive()
    }
}

impl<'a> Policy for PolicyHandle<'a> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.inner.is_authorized(policy_id, account)
    }
}
