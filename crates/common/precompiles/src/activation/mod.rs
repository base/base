//! Runtime activation registry native precompile.

mod abi;
pub use abi::IActivationRegistry;

mod storage;
pub use storage::ActivationRegistry;

mod dispatch;

mod precompile;
pub use precompile::ActivationRegistryPrecompile;
