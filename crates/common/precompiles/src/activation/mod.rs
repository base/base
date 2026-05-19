//! Runtime activation registry native precompile.

mod abi;
pub use abi::IActivationRegistry;

mod storage;
pub use storage::{
    ACTIVATION_ADMIN_ADDRESS, ACTIVATION_REGISTRY_ADDRESS, ActivationRegistry,
    ActivationRegistryStorage, SECURITIES_TOKEN_CREATION,
};

mod dispatch;

mod precompile;
