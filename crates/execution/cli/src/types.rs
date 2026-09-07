//! Concrete Base types used by offline CLI commands.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseEvmConfig;
use base_node_core::{BaseEngineTypes, BaseNetworkPrimitives, BaseStorage};
use reth_cli_commands::common::CliNodeTypes;
use reth_node_builder::NodeTypes;

/// Base storage, execution, and network configuration for offline commands.
#[derive(Debug, Clone)]
pub struct BaseCliTypes;

impl NodeTypes for BaseCliTypes {
    type ChainSpec = BaseChainSpec;
    type Storage = BaseStorage;
    type Payload = BaseEngineTypes;
}

impl CliNodeTypes for BaseCliTypes {
    type Evm = BaseEvmConfig;
    type Consensus = Arc<BaseBeaconConsensus>;
    type NetworkPrimitives = BaseNetworkPrimitives;
}

/// Concrete execution components used by Base maintenance commands.
pub type BaseCliComponents = reth_cli_commands::CliNodeComponents<BaseCliTypes>;
