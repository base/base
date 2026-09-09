#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod config;
pub use config::{
    Bootnodes, ChainConfig, DEVNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
    MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS, SEPOLIA_BERYL_ACTIVATION_ADMIN_ADDRESS,
    ZERONET_BERYL_ACTIVATION_ADMIN_ADDRESS,
};

mod upgrade;

mod upgrades;
pub use upgrades::Upgrades;

mod schedule_impl;

mod macros;
pub use macros::RollupConfigSource;

mod ethereum;
pub use ethereum::{Devnet, Holesky, Hoodi, L1_CONFIGS, Mainnet, Sepolia};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod params;
pub use params::FeeConfig;

mod updates;
pub use updates::{
    BatcherUpdate, DaFootprintGasScalarUpdate, Eip1559Update, GasConfigUpdate, GasLimitUpdate,
    MinBaseFeeUpdate, OperatorFeeUpdate, UnsafeBlockSignerUpdate, UpdateDataValidator,
    ValidatedUpdateData, ValidationError, Validator,
};

mod system;
pub use system::{
    BatcherUpdateError, DaFootprintGasScalarUpdateError, EIP1559UpdateError, GasConfigUpdateError,
    GasLimitUpdateError, LogProcessingError, MinBaseFeeUpdateError, OperatorFeeUpdateError,
    SystemConfig, SystemConfigLog, SystemConfigUpdate, SystemConfigUpdateError,
    SystemConfigUpdateKind, UnsafeBlockSignerUpdateError,
};

mod chain;
pub use chain::{
    AddressList, BaseUpgrade, BaseUpgradeConfig, Roles, RuntimeUpgradeRegistry,
    RuntimeUpgradeRegistryEntry, UpgradeActivation, UpgradeActivationOverrides,
    UpgradeActivationSink, UpgradeConfig,
};

mod genesis;
pub use genesis::ChainGenesis;

mod rollup;
pub use rollup::RollupConfig;

mod schedule;
pub use schedule::ChainUpgrades;

mod fork;
pub use fork::ExecutionFork;

mod basefee;
pub use basefee::*;

mod builder;
pub use builder::BaseChainSpecBuilder;

mod spec;
pub use spec::{BaseChainSpec, BaseChainSpecError, GenesisInfo};

mod provider;
pub use provider::ChainSpecProvider;

mod display;
pub use display::UpgradeDisplay;
