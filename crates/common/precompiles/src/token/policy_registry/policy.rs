//! Business logic for the `PolicyStorage` precompile.
//!
//! `Policy` is the concrete type the token holds. It wraps [`PolicyRegistryStorage`]
//! and implements the [`PolicyStorage`] trait, separating the authorization
//! decisions (here) from the raw storage reads (`storage.rs`).

use core::fmt;

use alloy_primitives::Address;
use base_precompile_storage::{Result, StorageCtx};

use super::storage::{PolicyRegistryStorage, PolicyStorage};

/// Concrete policy handle — wraps [`PolicyRegistryStorage`] and exposes
/// authorization decisions to the token layer.
pub struct Policy<'a> {
    inner: PolicyRegistryStorage<'a>,
}

impl fmt::Debug for Policy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Policy").finish_non_exhaustive()
    }
}

impl<'a> Policy<'a> {
    /// Creates a `Policy` backed by the registry storage at its singleton address.
    pub fn new(ctx: StorageCtx<'a>) -> Self {
        Self { inner: PolicyRegistryStorage::new(ctx) }
    }

    fn check_authorized(&self, _policy_id: u64, _account: Address) -> Result<bool> {
        Ok(true)
    }
}

impl PolicyStorage for Policy<'_> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.check_authorized(policy_id, account)
    }
}


