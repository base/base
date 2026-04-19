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
pub use args::TxpoolOrdering;

/// Exports Base-specific implementations of the [`EngineTypes`](reth_node_api::EngineTypes)
/// trait.
pub mod engine;
pub use engine::BaseEngineTypes;

pub mod node;
pub use node::*;

pub mod rpc;
pub use rpc::BaseEngineApiBuilder;

pub mod version;
pub use version::CLIENT_NAME;

pub mod proof_history;

/// Helpers for running test node instances.
#[cfg(feature = "test-utils")]
pub mod utils;

use base_common_evm as _;
#[cfg(feature = "test-utils")]
use reth_tasks as _;
use revm as _;
