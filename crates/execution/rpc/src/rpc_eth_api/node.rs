//! Components used by RPC handlers.

use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_execution_chainspec::ChainSpecProvider;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{PoolTransaction, TransactionPool};
use reth_chain_state::CanonStateSubscriptions;
use reth_network_api::NetworkInfo;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_types::EthStateCache;
use reth_storage_api::{
    BalProvider, BlockReader, BlockReaderIdExt, PruneCheckpointReader, StageCheckpointReader,
    StateProviderFactory,
};

/// Components used by RPC handlers.
pub trait RpcNodeCore: Clone + Send + Sync + Unpin + 'static {
    /// The provider type used to interact with the node.
    type Provider: BlockReaderIdExt<Block = BaseBlock, Receipt = BaseReceipt, Transaction = BaseTxEnvelope>
        + ChainSpecProvider
        + StateProviderFactory
        + CanonStateSubscriptions
        + StageCheckpointReader
        + PruneCheckpointReader
        + BalProvider
        + Send
        + Sync
        + Clone
        + Unpin
        + 'static;
    /// The transaction pool of the node.
    type Pool: TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>>;

    /// Network API.
    type Network: NetworkInfo + Clone;

    /// Returns the transaction pool of the node.
    fn pool(&self) -> &Self::Pool;

    /// Returns the node's evm config.
    fn evm_config(&self) -> &BaseEvmConfig;

    /// Returns the handle to the network
    fn network(&self) -> &Self::Network;

    /// Returns the provider of the node.
    fn provider(&self) -> &Self::Provider;
}

impl RpcNodeCore for base_node_context::BaseNodeContext {
    type Provider = BlockchainProvider;
    type Pool = base_node_context::BaseNodePool<BlockchainProvider>;

    type Network = reth_network::NetworkHandle;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        &self.transaction_pool
    }

    #[inline]
    fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        &self.network
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        &self.provider
    }
}

/// Additional components, asides the core node components, needed to run `eth_` namespace API
/// server.
pub trait RpcNodeCoreExt: RpcNodeCore<Provider: BlockReader> {
    /// Returns handle to RPC cache service.
    fn cache(&self) -> &EthStateCache;
}

/// An adapter that allows to construct [`RpcNodeCore`] from components.
#[derive(Debug, Clone)]
pub struct RpcNodeCoreAdapter<Provider, Pool, Network> {
    provider: Provider,
    pool: Pool,
    network: Network,
    evm_config: BaseEvmConfig,
}

impl<Provider, Pool, Network> RpcNodeCoreAdapter<Provider, Pool, Network> {
    /// Creates a new `RpcNodeCoreAdapter` instance.
    pub const fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        evm_config: BaseEvmConfig,
    ) -> Self {
        Self { provider, pool, network, evm_config }
    }
}

impl<Provider, Pool, Network> RpcNodeCore for RpcNodeCoreAdapter<Provider, Pool, Network>
where
    Provider: BlockReaderIdExt<Block = BaseBlock, Receipt = BaseReceipt, Transaction = BaseTxEnvelope>
        + ChainSpecProvider
        + StateProviderFactory
        + CanonStateSubscriptions
        + StageCheckpointReader
        + PruneCheckpointReader
        + BalProvider
        + Send
        + Sync
        + Unpin
        + Clone
        + 'static,
    Pool:
        TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>> + Unpin + 'static,
    Network: NetworkInfo + Clone + Unpin + 'static,
{
    type Provider = Provider;
    type Pool = Pool;

    type Network = Network;

    fn pool(&self) -> &Self::Pool {
        &self.pool
    }

    fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    fn network(&self) -> &Self::Network {
        &self.network
    }

    fn provider(&self) -> &Self::Provider {
        &self.provider
    }
}
