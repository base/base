//! Runtime activation registry native precompile.

mod abi;
pub use abi::IActivationRegistry;

mod storage;
pub use storage::{ActivationFeature, ActivationRegistryStorage};

mod dispatch;

mod precompile;
pub use precompile::ActivationRegistry;
