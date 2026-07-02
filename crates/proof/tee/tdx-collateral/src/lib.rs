#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::TdxAttestationConfig;

mod error;
pub use error::{Result, TdxCollateralError};

mod collateral;
pub use collateral::{TdxAttestationHydrator, TdxCollateralFetch};
