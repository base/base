//! Differential fuzzing across real nodes that follow the same synthetic chain: JSON-RPC answers
//! (`rpc`) and the state they compute and persist through reorgs and restarts (`state`).

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

mod process;
pub use process::NodeProcess;

mod state_ops;
pub use state_ops::{StateInput, StateMutator, StateOp};

mod state_session;
pub use state_session::{ChainTree, Divergence, StateSession, TreeBlock};

mod state_campaign;
pub use state_campaign::{StateCampaign, StateConfig, StateFinding, StateSummary};
