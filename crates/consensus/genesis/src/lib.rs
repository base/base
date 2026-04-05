#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod params;
pub use params::BaseFeeConfig;

mod updates;
pub use updates::{
    BatcherUpdate, DaFootprintGasScalarUpdate, Eip1559Update, GasConfigUpdate, GasLimitUpdate,
    MinBaseFeeUpdate, OperatorFeeUpdate, UnsafeBlockSignerUpdate,
};

mod system;
pub use system::{
    BatcherUpdateError, CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC,
    DaFootprintGasScalarUpdateError, EIP1559UpdateError, GasConfigUpdateError, GasLimitUpdateError,
    LogProcessingError, MinBaseFeeUpdateError, OperatorFeeUpdateError, SystemConfig,
    SystemConfigLog, SystemConfigUpdate, SystemConfigUpdateError, SystemConfigUpdateKind,
    UnsafeBlockSignerUpdateError,
};

mod chain;
pub use chain::{AddressList, BaseHardforkConfig, HardForkConfig, Roles};

mod genesis;
pub use genesis::ChainGenesis;

mod rollup;
pub use rollup::RollupConfig;
