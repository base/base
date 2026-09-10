#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(feature = "test-utils")]
use base_execution_state_database as _;

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
pub use payload_service::{BasePayloadServiceConfig, BasePayloadServiceMode};

#[cfg(test)]
mod storage;

mod version;
pub use version::CLIENT_NAME;

mod proof_history;
#[cfg(feature = "test-utils")]
use base_common_runtime_tasks as _;
use base_execution_evm_runtime as _;
pub use proof_history::*;

mod rpc;
pub use rpc::*;

mod exex;
pub use exex::*;

mod setup;
pub use setup::*;

mod txpool_prewarm;
pub use txpool_prewarm::*;

mod launch;
pub use launch::*;

mod engine_shutdown;
pub use engine_shutdown::{EngineShutdown, EngineShutdownRequest};

mod full_node;
pub use full_node::*;

mod launch_config;
pub use launch_config::NodeLaunch;
mod startup_context;
pub use startup_context::BuilderContext;

mod launch_components;
pub use launch_components::*;

mod handle;

pub use base_node_config::NodeConfig;
pub use handle::*;

mod rpc_services;
pub use rpc_services::BaseRpcServices;

mod upgrade_signal;
pub use upgrade_signal::{
    ExecutionUpgradeSignal, ExecutionUpgradeSignalConfig, ExecutionUpgradeSignalRuntime,
    RuntimeForkFilterNetwork,
};
mod services;
pub use services::{NodeServices, PreparedNodeServices, ShadowIndexerRuntime};

mod context;
pub use context::*;

mod node_events;
pub use node_events::*;

mod node_metrics;
pub use node_metrics::*;

mod ethstats;
pub use ethstats::*;
