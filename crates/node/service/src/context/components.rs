//! Shared components of a running Base node.

use base_common_runtime::TaskExecutor;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_provider::providers::BlockchainProvider;
use base_execution_txpool::BaseTransactionPool;

/// Container for the node's types and the components and other internals that can be used by
/// addons of the node.
#[derive(Debug, Clone)]
pub struct BaseNodeContext {
    /// The node transaction pool.
    pub transaction_pool: BaseTransactionPool,
    /// The Base EVM configuration.
    pub evm_config: BaseEvmConfig,
    /// The Base consensus validator.
    pub consensus: std::sync::Arc<base_execution_evm_blocks::BaseBeaconConsensus>,
    /// The network handle.
    pub network: base_execution_network_service::NetworkHandle,
    /// The payload service handle.
    pub payload_builder_handle: base_execution_payload::PayloadBuilderHandle,
    /// The task executor for the node.
    pub task_executor: TaskExecutor,
    /// The provider of the node.
    pub provider: BlockchainProvider,
}

impl BaseNodeContext {
    /// Returns the Base transaction pool.
    pub fn pool(&self) -> &BaseTransactionPool {
        &self.transaction_pool
    }

    /// Returns the Base EVM configuration.
    pub fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    /// Returns the Base consensus validator.
    pub fn consensus(&self) -> &std::sync::Arc<base_execution_evm_blocks::BaseBeaconConsensus> {
        &self.consensus
    }

    /// Returns the network handle.
    pub fn network(&self) -> &base_execution_network_service::NetworkHandle {
        &self.network
    }

    /// Returns the payload service handle.
    pub fn payload_builder_handle(&self) -> &base_execution_payload::PayloadBuilderHandle {
        &self.payload_builder_handle
    }

    /// Returns the blockchain provider.
    pub fn provider(&self) -> &BlockchainProvider {
        &self.provider
    }

    /// Returns the node task executor.
    pub fn task_executor(&self) -> &TaskExecutor {
        &self.task_executor
    }
}
