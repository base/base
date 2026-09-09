//! Inputs used to construct the Base eth API.

use reth_engine_primitives::ConsensusEngineHandle;
use reth_rpc_eth_types::{EthConfig, EthStateCache};

/// Node components and RPC settings used to construct the Base eth API.
#[derive(Debug)]
pub struct EthApiCtx<'a> {
    /// Reference to the node components
    pub components: &'a base_node_context::BaseNodeContext,
    /// Eth API configuration
    pub config: EthConfig,
    /// Cache for eth state
    pub cache: EthStateCache,
    /// Handle to the beacon consensus engine
    pub engine_handle: ConsensusEngineHandle,
}

impl<'a> EthApiCtx<'a> {
    /// Provides a [`crate::EthApiBuilder`] with preconfigured config and components.
    pub fn eth_api_builder(self) -> crate::EthApiBuilder {
        crate::EthApiBuilder::new_with_components(self.components.into())
            .eth_cache(self.cache)
            .task_spawner(self.components.task_executor().clone())
            .gas_cap(self.config.rpc_gas_cap.into())
            .max_simulate_blocks(self.config.rpc_max_simulate_blocks)
            .compute_state_root_for_eth_simulate(self.config.compute_state_root_for_eth_simulate)
            .eth_proof_window(self.config.eth_proof_window)
            .fee_history_cache_config(self.config.fee_history_cache)
            .proof_permits(self.config.proof_permits)
            .gas_oracle_config(self.config.gas_oracle)
            .max_batch_size(self.config.max_batch_size)
            .max_blocking_io_requests(self.config.max_blocking_io_requests)
            .pending_block_kind(self.config.pending_block_kind)
            .raw_tx_forwarder(self.config.raw_tx_forwarder)
            .evm_memory_limit(self.config.rpc_evm_memory_limit)
            .force_blob_sidecar_upcasting(self.config.force_blob_sidecar_upcasting)
    }
}
