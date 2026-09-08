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
pub use args::{
    DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS, HasRollupArgs, ProofsHistoryDbBackend,
    ProofsHistoryRocksdbArgs, RollupArgs, TWELVE_HOURS_IN_BLOCKS, TxpoolOrdering,
};

/// Base engine validation.
pub mod engine;

pub mod node;
pub use node::*;
pub use reth_node_builder::BaseEngineApiBuilder;

mod payload_service;
pub use payload_service::{BasePayloadServiceBuilder, BasePayloadServiceMode};

mod components;
pub use components::BaseComponentsBuilder;

#[cfg(test)]
mod storage;

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

mod add_ons;
pub use add_ons::{BaseAddOns, BaseAddOnsBuilder};
