//! Runtime activation registry native precompile.

pub use base_common_chains::{ActivationFeature, IActivationRegistry};

mod storage;
pub use storage::{ActivationAdminConfig, ActivationRegistryStorage};

mod dispatch;

mod precompile;
pub use precompile::ActivationRegistry;
