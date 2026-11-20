//! Runtime activation registry native precompile.

pub use base_precompile_abi::{ActivationFeature, IActivationRegistry};

mod storage;
pub use storage::{ActivationAdminConfig, ActivationRegistryStorage};

mod dispatch;

mod precompile;
pub use precompile::ActivationRegistry;
