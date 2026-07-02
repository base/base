#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod direct;
pub use direct::DirectProver;

mod error;
pub use error::{ProverError, Result};

#[cfg(feature = "prove")]
mod boundless;
#[cfg(feature = "prove")]
pub use boundless::BoundlessProver;
