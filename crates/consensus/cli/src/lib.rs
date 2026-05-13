#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod app;
pub use app::{ConsensusCli, ConsensusCommands, LogArgs, MetricsArgs};

mod bootnode;
pub use bootnode::{Bootnode, BootnodeEnr, BootnodeP2PArgs, resolve_host};

mod chain;
pub use chain::{ConsensusChainArgs, GlobalConsensusChainArgs};

mod config;
pub use config::{ConfigError, L1ConfigFile, L2ConfigFile};

mod follow;
pub use follow::{
    ConsensusFollowNodeArgs, ConsensusFollowNodeCommand, ConsensusFollowNodeConfigArgs,
    ConsensusFollowNodeOverrides,
};

mod l1;
pub use l1::L1ClientArgs;

mod l2;
pub use l2::L2ClientArgs;

mod metrics;
pub use metrics::CliMetrics;

mod node;
pub use node::{
    ConsensusNodeArgs, ConsensusNodeCommand, ConsensusNodeConfigArgs, ConsensusNodeOverrides,
};

mod rpc;
pub use rpc::RpcArgs;

mod sequencer;
pub use sequencer::SequencerArgs;

pub mod signer;
pub use signer::{SignerArgs, SignerArgsParseError};

pub mod p2p;
pub use p2p::{P2PArgs, P2PConfigError};
