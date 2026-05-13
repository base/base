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
mod args;
pub use args::{RollupArgs, TxpoolOrdering};

/// Exports Base-specific implementations of the [`EngineTypes`](reth_node_api::EngineTypes)
/// trait.
mod engine;
pub use engine::{
    BaseEngineTypes, BaseEngineValidator, BasePostExecutionValidator, validate_withdrawals_presence,
};

mod node;
pub use node::*;

mod rpc;
pub use rpc::BaseEngineApiBuilder;

mod storage;
pub use storage::BaseStorage;

mod version;
pub use version::CLIENT_NAME;

mod proof_history;
pub use proof_history::launch_node_with_proof_history;

/// Helpers for running test node instances.
#[cfg(feature = "test-utils")]
mod utils;
#[cfg(feature = "test-utils")]
pub use utils::{BaseNode as TestBaseNode, advance_chain, payload_attributes, setup};

use base_common_evm as _;
#[cfg(feature = "test-utils")]
use reth_tasks as _;
use revm as _;
