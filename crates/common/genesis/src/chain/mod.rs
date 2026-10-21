//! Module containing the chain config.

mod upgrade;
pub use upgrade::{
    BaseUpgrade, BaseUpgradeConfig, RuntimeUpgradeRegistry, RuntimeUpgradeRegistryEntry,
    UpgradeActivation, UpgradeActivationOverrides, UpgradeActivationSink, UpgradeConfig,
};
