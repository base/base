//! Base fixtures for the shared RPC implementation.

use base_execution_chainspec::ChainSpecProvider;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::BasePooledTransaction;
use reth_rpc_eth_api::{RpcNodeCore, node::RpcNodeCoreAdapter};
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, blobstore::InMemoryBlobStore, noop::MockTransactionValidator,
};

use crate::EthApiBuilder;

/// Pool accepting Base transactions in RPC tests.
pub type TestPool = Pool<
    MockTransactionValidator<BasePooledTransaction>,
    CoinbaseTipOrdering<BasePooledTransaction>,
    InMemoryBlobStore,
>;

/// Constructs the Base fixtures for shared RPC tests.
#[derive(Debug)]
pub struct RpcTestUtils;

impl RpcTestUtils {
    /// Creates a pool accepting Base transactions.
    pub fn pool() -> TestPool {
        Pool::new(
            MockTransactionValidator::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        )
    }

    /// Builds shared RPC handlers with the supplied provider and Base fixtures.
    pub fn api_builder<Provider, Network>(
        provider: Provider,
        pool: TestPool,
        network: Network,
        evm: BaseEvmConfig,
    ) -> EthApiBuilder<RpcNodeCoreAdapter<Provider, TestPool, Network>>
    where
        Provider: ChainSpecProvider,
        RpcNodeCoreAdapter<Provider, TestPool, Network>: RpcNodeCore<Provider = Provider>,
    {
        EthApiBuilder::new(provider, pool, network, evm)
    }
}
