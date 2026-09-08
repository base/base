//! Helper trait for interfacing with [`FullNodeComponents`].

use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_execution_chainspec::ChainSpecProvider;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{PoolTransaction, TransactionPool};
use base_node_context::FullNodeComponents;
use reth_chain_state::CanonStateSubscriptions;
use reth_network_api::NetworkInfo;
use reth_rpc_eth_types::EthStateCache;
use reth_storage_api::{
    BalProvider, BlockReader, BlockReaderIdExt, PruneCheckpointReader, StageCheckpointReader,
    StateProviderFactory,
};

/// Helper trait that provides the same interface as [`FullNodeComponents`] but without requiring
/// implementation of trait bounds.
///
/// This trait is structurally equivalent to [`FullNodeComponents`], exposing the same associated
/// types and methods. However, it doesn't enforce the trait bounds required by
/// [`FullNodeComponents`]. This makes it useful for RPC types that need access to node components
/// where the full trait bounds of the components are not necessary.
///
/// Every type that is a [`FullNodeComponents`] also implements this trait.
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

impl<T> RpcNodeCore for T
where
    T: FullNodeComponents<Provider: ChainSpecProvider>,
{
    type Provider = T::Provider;
    type Pool = base_node_context::BaseNodePool<T::Provider>;

    type Network = reth_network::NetworkHandle;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        FullNodeComponents::pool(self)
    }

    #[inline]
    fn evm_config(&self) -> &BaseEvmConfig {
        FullNodeComponents::evm_config(self)
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        FullNodeComponents::network(self)
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        FullNodeComponents::provider(self)
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
