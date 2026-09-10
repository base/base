//! Command-line arguments for Base node services.

mod app;
pub use app::{LogArgs, MetricsArgs};

mod bootnode;
pub use bootnode::{Bootnode, BootnodeEnr, BootnodeP2PArgs, resolve_host};

mod chain;
pub use chain::ConsensusChainArgs;

mod config;
pub use config::{ConfigError, L1ConfigFile, L2ConfigFile};

mod follow;
pub use follow::{ConsensusFollowNodeArgs, ConsensusFollowNodeConfigArgs, EmbeddedFollowArgs};

mod l1;
pub use l1::L1ClientArgs;

mod metrics;
pub use metrics::CliMetrics;

mod node;
pub use node::{
    ConsensusNodeArgs, ConsensusNodeConfigArgs, ConsensusNodeOverrides, ConsensusNodeStartOptions,
    EmbeddedConsensusNodeConfigArgs, EmbeddedSequencerConsensusNodeConfigArgs,
};

mod rpc;
pub use rpc::{EmbeddedRpcArgs, RpcArgs};

mod sequencer;
pub use sequencer::SequencerArgs;

mod signer;
pub use signer::{SignerArgs, SignerArgsParseError};

mod p2p;
pub use p2p::{EmbeddedP2PArgs, P2PArgs, P2PConfigError, P2PNetworkArgs};
