//! Runtime activation registry native precompile.

mod abi;
pub use abi::IActivationRegistry;

mod storage;
pub use storage::{ActivationRegistry, ActivationRegistryStorage};

mod dispatch;

mod precompile;
