//! Chain and state fixtures for integration tests.

pub mod generators;
mod genesis_allocator;
pub use genesis_allocator::GenesisAllocator;
mod base;
pub use base::BaseTestData;
