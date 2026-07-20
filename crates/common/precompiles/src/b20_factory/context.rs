//! Contract context for the `B20Factory` precompile.
//!
//! [`FactoryContractContext`] is the minimal storage holder the logic and dispatcher
//! operate on. It carries no business logic of its own — behavior lives in the
//! version implementations resolved from [`super::VersionResolver`].

use core::fmt;

use super::B20FactoryStorage;

/// Storage binding the factory logic operates on.
///
/// A minimal `storage` holder; it carries no behavior of its own — all business
/// logic lives in the version implementations resolved from
/// [`super::VersionResolver`].
pub struct FactoryContractContext<'a> {
    storage: B20FactoryStorage<'a>,
}

impl fmt::Debug for FactoryContractContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FactoryContractContext").finish_non_exhaustive()
    }
}

impl<'a> FactoryContractContext<'a> {
    /// Creates a context backed by factory storage.
    pub const fn with_storage(storage: B20FactoryStorage<'a>) -> Self {
        Self { storage }
    }

    /// Returns a shared reference to the underlying storage.
    pub const fn storage(&self) -> &B20FactoryStorage<'a> {
        &self.storage
    }

    /// Returns an exclusive reference to the underlying storage.
    pub const fn storage_mut(&mut self) -> &mut B20FactoryStorage<'a> {
        &mut self.storage
    }
}
