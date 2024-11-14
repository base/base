//! Payload builder for the op-rbuilder with bundle support.

#![cfg_attr(all(not(test), feature = "optimism"), warn(unused_crate_dependencies))]
// The `optimism` feature must be enabled to use this crate.
#![cfg(feature = "optimism")]

pub mod builder;
pub use builder::OpRbuilderPayloadBuilder;
