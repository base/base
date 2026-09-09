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
pub use args::{
    DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS, ProofsHistoryDbBackend, ProofsHistoryRocksdbArgs,
    RollupArgs, TWELVE_HOURS_IN_BLOCKS, TxpoolOrdering, *,
};

/// Base engine validation.
mod engine;
pub use engine::*;

mod node;
pub use node::*;

mod payload_service;
pub use payload_service::{BasePayloadServiceBuilder, BasePayloadServiceMode};

mod components;
pub use components::BaseComponentsBuilder;

#[cfg(test)]
mod storage;

mod version;
pub use version::CLIENT_NAME;

mod proof_history;
use base_common_evm as _;
pub use proof_history::*;
#[cfg(feature = "test-utils")]
use reth_tasks as _;
use revm as _;

mod add_ons;
pub use add_ons::{BaseAddOns, BaseAddOnsBuilder};

mod rpc;
pub use rpc::*;

mod exex;
pub use exex::*;

mod setup;
pub use setup::*;

mod txpool_prewarm;
pub use txpool_prewarm::*;

mod hooks;
pub use hooks::*;

mod launch;
pub use launch::*;

mod engine_shutdown;
pub use engine_shutdown::{EngineShutdown, EngineShutdownRequest};

mod full_node;
pub use full_node::*;

mod builder;
pub use builder::*;

mod aliases;
pub use aliases::*;

mod launch_components;
pub use launch_components::*;

mod handle;

pub use handle::*;
pub use reth_node_core::{
    cli::config::{PayloadBuilderConfig, RethNetworkConfig, RethTransactionPoolConfig},
    node_config::NodeConfig,
};

mod rpc_services;
pub use rpc_services::BaseRpcServices;
