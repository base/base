//! Module containing the chain config.

mod upgrade;
pub use upgrade::{
    BaseUpgrade, BaseUpgradeConfig, UpgradeActivation, UpgradeActivationOverrides,
    UpgradeActivationSink, UpgradeConfig,
};

#[cfg(feature = "runtime-registry")]
mod registry;
#[cfg(feature = "runtime-registry")]
pub use registry::{RuntimeUpgradeRegistry, RuntimeUpgradeRegistryEntry};
