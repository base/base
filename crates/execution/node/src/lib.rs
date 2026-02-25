//! Standalone crate for Optimism-specific Reth configuration and builder types.
//!
//! # features
//! - `js-tracer`: Enable the `JavaScript` tracer for the `debug_trace` endpoints

#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(feature = "test-utils")]
use reth_db_api as _;

/// CLI argument parsing for the optimism node.
pub mod args;

/// Exports optimism-specific implementations of the [`EngineTypes`](reth_node_api::EngineTypes)
/// trait.
pub mod engine;
pub use engine::OpEngineTypes;

pub mod node;
pub use node::{
    BaseLocalPayloadAttributesBuilder, OpAddOns, OpAddOnsBuilder, OpConsensusBuilder,
    OpEngineValidatorBuilder, OpExecutorBuilder, OpFullNodeTypes, OpNetworkBuilder,
    OpNetworkPrimitives, OpNode, OpNodeComponentBuilder, OpNodeTypes, OpPoolBuilder,
};

pub mod rpc;
pub use rpc::OpEngineApiBuilder;

pub mod version;
pub use reth_optimism_txpool as txpool;
pub use version::OP_NAME_CLIENT;

pub mod proof_history;

/// Helpers for running test node instances.
#[cfg(feature = "test-utils")]
pub mod utils;

use base_revm as _;
pub use reth_optimism_evm::{
    L1BlockInfoError, OpBlockAssembler, OpBlockExecutionCtx, OpBlockExecutionError,
    OpBlockExecutorFactory, OpEvm, OpEvmConfig, OpEvmFactory, OpExecutorProvider,
    OpNextBlockEnvAttributes, OpRethReceiptBuilder, extract_l1_info, extract_l1_info_from_tx,
    parse_l1_info, parse_l1_info_tx_bedrock, revm_spec, revm_spec_by_timestamp_after_bedrock,
};
pub use reth_optimism_payload_builder::{
    self as payload, OpBuiltPayload, OpPayloadAttributes, OpPayloadBuilder,
    OpPayloadBuilderAttributes, OpPayloadPrimitives, OpPayloadTypes, config::OpDAConfig,
};
pub use reth_optimism_storage::OpStorage;
#[cfg(feature = "test-utils")]
use reth_tasks as _;
use revm as _;
