//! Concrete Base types used by offline CLI commands.

use std::sync::Arc;

use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseEvmConfig;
use reth_cli_commands::common::CliNodeTypes;

/// Base storage, execution, and network configuration for offline commands.
#[derive(Debug, Clone)]
pub struct BaseCliTypes;

impl CliNodeTypes for BaseCliTypes {
    type Evm = BaseEvmConfig;
    type Consensus = Arc<BaseBeaconConsensus>;
}

/// Concrete execution components used by Base maintenance commands.
pub type BaseCliComponents = reth_cli_commands::CliNodeComponents<BaseCliTypes>;
