//! Runtime activation registry native precompile.

mod abi;
pub use abi::IActivationRegistry;

mod storage;
pub use storage::{ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage};

mod dispatch;
