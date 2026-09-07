//! Execution components used by offline CLI commands.

use crate::common::CliNodeTypes;

/// Concrete execution and consensus components for CLI commands.
#[derive(Debug)]
pub struct CliNodeComponents<N: CliNodeTypes> {
    /// EVM used to execute blocks.
    pub evm_config: N::Evm,
    /// Consensus used to validate blocks.
    pub consensus: N::Consensus,
}
