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
    BatcherUpdate, DaFootprintGasScalarUpdate, EXPECTED_DATA_LENGTH, EXPECTED_POINTER,
    Eip1559Update, GasConfigUpdate, GasLimitUpdate, MinBaseFeeUpdate, OperatorFeeUpdate,
    STANDARD_UPDATE_DATA_LEN, UnsafeBlockSignerUpdate, ValidatedUpdateData, ValidationError,
    validate_update_data,
};

mod system;
pub use system::{
    BatcherUpdateError, DaFootprintGasScalarUpdateError, EIP1559UpdateError, GasConfigUpdateError,
    GasLimitUpdateError, LogProcessingError, MinBaseFeeUpdateError, OperatorFeeUpdateError,
    SystemConfig, SystemConfigLog, SystemConfigUpdate, SystemConfigUpdateError,
    SystemConfigUpdateKind, UnsafeBlockSignerUpdateError,
};

mod chain;
pub use chain::{AddressList, BaseHardforkConfig, HardForkConfig, Roles};

mod genesis;
pub use genesis::ChainGenesis;

mod rollup;
pub use rollup::RollupConfig;
