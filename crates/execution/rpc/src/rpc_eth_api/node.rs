//! Concrete components used by Base RPC handlers.

use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_network_service::NetworkHandle;
use base_execution_state_provider::providers::BlockchainProvider;
use base_execution_txpool::BaseTransactionPool;

/// The provider, pool, network, and execution rules shared by Base RPC handlers.
#[derive(Debug, Clone)]
pub struct BaseRpcContext {
    /// Canonical Base blockchain and state provider.
    pub provider: BlockchainProvider,
    /// Base transaction pool.
    pub pool: BaseTransactionPool,
    /// Handle to the peer network.
    pub network: NetworkHandle,
    /// Base execution rules.
    pub evm_config: BaseEvmConfig,
}
