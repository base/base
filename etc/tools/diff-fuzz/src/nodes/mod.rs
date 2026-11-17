//! Differential JSON-RPC fuzzing across real nodes that follow the same synthetic chain.

mod client;
pub use client::{CoverageReading, NodeClient, Reply};

mod genesis;
pub use genesis::NodeGenesis;

mod follow;
pub use follow::{ChainBlock, ChainFollower, ChainIndex, ChainTx, Lag};

mod requests;
pub use requests::{FAMILIES, RpcCase, RpcGen};

mod compare;
pub use compare::{RpcCompare, RpcDifference, RpcVerdict};

mod campaign;
pub use campaign::{MethodStats, RpcCampaign, RpcConfig, RpcExclude, RpcFinding, RpcSummary};

mod mutate;
pub use mutate::{PathStep, RpcMutator};

mod guide;
pub use guide::{GuideReport, GuideSample, GuideState, RpcGuide};
