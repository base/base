#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Activate reth-optimism-node features
use reth_optimism_node as _;

mod builder;
mod clients;
mod error;
mod handle;
mod node;

// Re-export key types from dependencies
pub use base_engine_actor::{
    DirectEngineActor, DirectEngineApi, DirectEngineProcessor, EngineActorRequest,
};
pub use base_engine_ext::{
    ConsensusEngineHandle, InProcessEngineClient, OpEngineTypes, PayloadStore,
};

// Suppress unused dependency warnings for future use
use kona_derive as _;
use kona_engine as _;
pub use kona_genesis::RollupConfig;

// Re-export kona actor types for wiring
pub use kona_node_service::{
    // Derivation actor
    DerivationActor,
    DerivationEngineClient,
    // L1 watcher actor
    L1WatcherActor,
    // Network actor
    NetworkActor,
    NetworkConfig,
    NetworkEngineClient,
    NetworkInboundData,
    // Node actor trait
    NodeActor,
};
use op_alloy_rpc_types_engine as _;

// Re-export crate types
pub use builder::UnifiedRollupNodeBuilder;
pub use clients::EngineClients;
pub use error::UnifiedRollupNodeError;
pub use handle::NodeHandle;
pub use node::UnifiedRollupNode;
