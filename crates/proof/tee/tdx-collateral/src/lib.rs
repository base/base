#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{DEFAULT_TDX_TRUSTED_ROOT_CA_HASH, TdxAttestationConfig};

mod error;
pub use error::{BoxError, Result, TdxCollateralError};

mod collateral;
pub use collateral::{TdxAttestationHydrator, TdxCollateralFetch};
