//! Module containing the chain config.

mod addresses;
pub use addresses::AddressList;

mod upgrade;
pub use upgrade::{
    BaseUpgrade, BaseUpgradeConfig, ProcessedHeadReservation, RuntimeUpgradeRegistry,
    RuntimeUpgradeRegistryEntry, UpgradeActivation, UpgradeActivationOverrides,
    UpgradeActivationSink, UpgradeConfig,
};

mod roles;
pub use roles::Roles;
