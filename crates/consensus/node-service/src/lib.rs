#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod service;

// Re-export Base node types
// Suppress unused dependency warnings for crates used only in sub-modules
use alloy_rpc_types_engine as _;
// Re-export kona-genesis for rollup configuration
pub use kona_genesis::RollupConfig;
// Re-export metrics
#[cfg(feature = "metrics")]
pub use kona_node_service::Metrics;
// Re-export kona-node-service types that we don't wrap
pub use kona_node_service::{
    // Core types
    DerivationDelegateConfig,
    InteropMode,
    NetworkConfig,
    NodeMode,
    // Sequencer types
    SequencerConfig,
};
pub use service::{BaseEngineConfig, BaseNode, BaseNodeBuilder, L1Config, L1ConfigBuilder};
use thiserror as _;
