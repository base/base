#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(not(test))]
use tokio as _;

mod cli;
pub use cli::{Cli, CollateralArgs};

mod report;
pub use report::{
    OnchainRegistryReport, QuoteVerificationReport, TdxImageHashReport, TdxMeasurementsReport,
};

mod tool;
pub use tool::{OnchainRegistryConfig, TdxImageHashConfig, TdxImageHashTool};
