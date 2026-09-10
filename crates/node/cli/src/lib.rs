#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Base chain specification parser.
pub mod chainspec;
/// Base CLI commands.
pub mod commands;
mod types;
pub use types::BaseCliComponents;

mod node;
pub use node::{
    ExecutionNodeArgs, ExecutionNodeConfigArgs, ExecutionNodeLaunchConfig,
    ExecutionNodeRuntimeConfig,
};
/// Standard Base execution-node runner wiring.
mod standard_node;

pub use base_node_service::{
    ExecutionUpgradeSignal, ExecutionUpgradeSignalConfig, RuntimeForkFilterNetwork,
};
pub use standard_node::{
    MeteringArgs, ResourceMeteringArgs, RpcStandardNodeArgs, ShadowIndexerArgs,
    StandardBaseRethNode, StandardNodeArgs,
};

mod batcher;
pub use batcher::{BatcherArgs, SignerCli};

mod builder;
pub use builder::{BuilderArgs, TransactionEventsArgs};
