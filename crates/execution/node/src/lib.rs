#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(feature = "test-utils")]
use reth_db_api as _;

/// CLI argument parsing for the Base node.
pub mod args;

/// Exports Base-specific implementations of the [`EngineTypes`](reth_node_api::EngineTypes)
/// trait.
pub mod engine;
pub use engine::OpEngineTypes;

pub mod node;
pub use node::*;

pub mod rpc;
pub use rpc::OpEngineApiBuilder;

pub mod version;
pub use version::OP_NAME_CLIENT;

pub mod proof_history;

/// Helpers for running test node instances.
#[cfg(feature = "test-utils")]
pub mod utils;

pub use base_execution_evm::*;
pub use base_execution_payload_builder::{
    self as payload, OpBuiltPayload, OpPayloadAttributes, OpPayloadBuilder,
    OpPayloadBuilderAttributes, OpPayloadPrimitives, OpPayloadTypes, config::OpDAConfig,
};
pub use base_execution_storage::OpStorage;
use base_revm as _;
#[cfg(feature = "test-utils")]
use reth_tasks as _;
use revm as _;
