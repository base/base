#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod components;
pub use components::{BaseNodeContext, BaseNodePool, FullNodeComponents};
mod add_ons;
pub use add_ons::{AddOnsContext, NodeAddOns};
