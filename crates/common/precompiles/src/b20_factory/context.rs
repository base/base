//! Contract context for the `B20Factory` precompile.
//!
//! [`ContractContext`] is the minimal storage holder the logic and dispatcher
//! operate on. It carries no business logic of its own — behavior lives in the
//! version implementations resolved from [`super::VersionResolver`].

use super::B20FactoryStorage;

/// Storage binding the factory logic operates on.
///
/// A minimal `storage` holder; it carries no behavior of its own — all business
/// logic lives in the version implementations resolved from
/// [`super::VersionResolver`].
pub struct ContractContext<'a> {
    storage: B20FactoryStorage<'a>,
}

impl<'a> ContractContext<'a> {
    /// Creates a context backed by factory storage.
    pub const fn with_storage(storage: B20FactoryStorage<'a>) -> Self {
        Self { storage }
    }

    /// Returns a shared reference to the underlying storage.
    pub fn storage(&self) -> &B20FactoryStorage<'a> {
        &self.storage
    }

    /// Returns an exclusive reference to the underlying storage.
    pub fn storage_mut(&mut self) -> &mut B20FactoryStorage<'a> {
        &mut self.storage
    }
}
