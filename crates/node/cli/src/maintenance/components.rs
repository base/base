//! Execution components used by offline CLI commands.

use std::sync::Arc;

use base_execution_evm_blocks::{BaseBeaconConsensus, BaseEvmConfig};

/// Concrete execution and consensus components for CLI commands.
#[derive(Debug)]
pub struct CliNodeComponents {
    /// EVM used to execute blocks.
    pub evm_config: BaseEvmConfig,
    /// Consensus used to validate blocks.
    pub consensus: Arc<BaseBeaconConsensus>,
}
