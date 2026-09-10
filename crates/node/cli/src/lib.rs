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
pub use chainspec::BaseChainSpecParser;
/// Base CLI commands.
pub mod commands;

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

mod consensus;
pub use consensus::{
    Bootnode, BootnodeEnr, BootnodeP2PArgs, CliMetrics, ConfigError, ConsensusChainArgs,
    ConsensusFollowNodeArgs, ConsensusFollowNodeConfigArgs, ConsensusNodeArgs,
    ConsensusNodeConfigArgs, ConsensusNodeOverrides, ConsensusNodeStartOptions,
    EmbeddedConsensusNodeConfigArgs, EmbeddedFollowArgs, EmbeddedP2PArgs, EmbeddedRpcArgs,
    EmbeddedSequencerConsensusNodeConfigArgs, L1ClientArgs, L1ConfigFile, L2ConfigFile, LogArgs,
    MetricsArgs, P2PArgs, P2PConfigError, P2PNetworkArgs, RpcArgs, SequencerArgs, SignerArgs,
    SignerArgsParseError, resolve_host,
};

mod maintenance;
pub use maintenance::*;
