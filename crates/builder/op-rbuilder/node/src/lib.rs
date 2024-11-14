//! Standalone crate for op-rbuilder-specific Reth configuration and builder types.
//!
//! Inherits Network, Executor, and Consensus Builders from the Optimism defaults,
//! overrides the Pool and Payload Builders.

#![cfg_attr(all(not(test), feature = "optimism"), warn(unused_crate_dependencies))]
// The `optimism` feature must be enabled to use this crate.
#![cfg(feature = "optimism")]

use tracing as _;

/// CLI argument parsing.
pub mod args;

pub mod node;
pub use node::OpRbuilderNode;
