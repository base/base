//! Concrete components used by Base RPC handlers.

use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_provider::providers::BlockchainProvider;
use reth_network::NetworkHandle;
use {base_execution_txpool::BaseTransactionPool, base_node_context::BaseNodeContext};

/// The provider, pool, network, and execution rules shared by Base RPC handlers.
#[derive(Debug, Clone)]
pub struct BaseRpcContext {
    /// Canonical Base blockchain and state provider.
    pub provider: BlockchainProvider,
    /// Base transaction pool.
    pub pool: BaseTransactionPool<BlockchainProvider>,
    /// Handle to the peer network.
    pub network: NetworkHandle,
    /// Base execution rules.
    pub evm_config: BaseEvmConfig,
}

impl From<&BaseNodeContext> for BaseRpcContext {
    fn from(node: &BaseNodeContext) -> Self {
        Self {
            provider: node.provider.clone(),
            pool: node.transaction_pool.clone(),
            network: node.network.clone(),
            evm_config: node.evm_config.clone(),
        }
    }
}
