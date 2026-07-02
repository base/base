#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::TdxAttestationConfig;

mod collateral;
pub use collateral::{TdxAttestationHydrator, TdxCollateralFetch};
