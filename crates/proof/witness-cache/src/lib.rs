#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod attributes;
pub use attributes::{PayloadAttributes, PayloadAttributesError};

mod cache;
pub use cache::{DEFAULT_WITNESS_CACHE_BLOCKS, WitnessCache, WitnessKey};

mod metrics;
pub use metrics::Metrics;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::{WitnessCacheClient, WitnessCacheError};

#[cfg(feature = "producer")]
mod producer;
#[cfg(feature = "producer")]
pub use producer::WitnessFollower;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::WitnessServer;
