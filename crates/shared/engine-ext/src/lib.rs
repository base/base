#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod api;

mod client;
pub use client::InProcessEngineClient;

mod error;
pub use error::EngineError;

mod forkchoice;
mod payload;
mod query;

mod state;
// Re-export types needed to construct the client
pub use reth_engine_primitives::ConsensusEngineHandle;
pub use reth_optimism_node::OpEngineTypes;
pub use reth_payload_builder::PayloadStore;
pub use state::ForkchoiceTracker;
