//! Concrete components used by Base RPC handlers.

use base_execution_evm::BaseEvmConfig;
use base_node_context::{BaseNodeContext, BaseNodePool};
use reth_network::NetworkHandle;
use reth_provider::providers::BlockchainProvider;

/// The provider, pool, network, and execution rules shared by Base RPC handlers.
#[derive(Debug, Clone)]
pub struct BaseRpcContext {
    /// Canonical Base blockchain and state provider.
    pub provider: BlockchainProvider,
    /// Base transaction pool.
    pub pool: BaseNodePool<BlockchainProvider>,
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
