//! Contains the Base node configuration structures.

use base_primitives::{FlashblocksCell, FlashblocksConfig, OpProvider, TracingConfig};
use base_reth_flashblocks::{FlashblocksCanonConfig, FlashblocksState};
use base_reth_rpc::BaseRpcConfig;
use base_txpool::TransactionTracingConfig;
use reth_optimism_node::args::RollupArgs;

/// Concrete type alias for the flashblocks cell used in the runner.
pub type RunnerFlashblocksCell = FlashblocksCell<FlashblocksState<OpProvider>>;

/// Captures the pieces of CLI configuration that the node logic cares about.
#[derive(Debug, Clone)]
pub struct BaseNodeConfig {
    /// Rollup-specific arguments forwarded to the Optimism node implementation.
    pub rollup_args: RollupArgs,
    /// Optional flashblocks configuration if the websocket URL was provided.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Execution extension tracing toggles.
    pub tracing: TracingConfig,
    /// Indicates whether the metering RPC surface should be installed.
    pub metering_enabled: bool,
    /// Shared Flashblocks state cache.
    pub flashblocks_cell: RunnerFlashblocksCell,
}

impl BaseNodeConfig {
    /// Returns `true` if flashblocks support should be wired up.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.flashblocks.is_some()
    }
}

// Implement configuration traits for BaseNodeConfig so it can be used
// with ConfigurableBaseNodeExtension

impl FlashblocksCanonConfig for BaseNodeConfig {
    fn flashblocks_cell(&self) -> &FlashblocksCell<FlashblocksState<OpProvider>> {
        &self.flashblocks_cell
    }

    fn flashblocks(&self) -> Option<&FlashblocksConfig> {
        self.flashblocks.as_ref()
    }
}

impl TransactionTracingConfig for BaseNodeConfig {
    fn tracing(&self) -> &TracingConfig {
        &self.tracing
    }
}

impl BaseRpcConfig for BaseNodeConfig {
    fn flashblocks_cell(&self) -> &FlashblocksCell<FlashblocksState<OpProvider>> {
        &self.flashblocks_cell
    }

    fn flashblocks(&self) -> Option<&FlashblocksConfig> {
        self.flashblocks.as_ref()
    }

    fn metering_enabled(&self) -> bool {
        self.metering_enabled
    }

    fn sequencer_rpc(&self) -> Option<&str> {
        self.rollup_args.sequencer.as_deref()
    }
}
