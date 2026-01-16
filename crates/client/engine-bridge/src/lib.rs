#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// This import is needed to activate the proper feature flags for reth-optimism-primitives
use reth_optimism_node as _;

mod driver;
mod forkchoice;
mod payload;
mod queries;

// Re-export core traits from engine-driver for convenience
pub use base_engine_driver::{DirectEngineApi, DirectEngineError};
pub use driver::InProcessEngineDriver;
// Re-export reth types needed to construct the driver
pub use reth_engine_primitives::ConsensusEngineHandle;
pub use reth_optimism_node::OpEngineTypes;
pub use reth_payload_builder::PayloadStore;
