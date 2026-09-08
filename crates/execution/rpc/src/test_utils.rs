//! Base fixtures for the shared RPC implementation.

use base_execution_chainspec::ChainSpecProvider;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    BasePooledTransaction, CoinbaseTipOrdering, InMemoryBlobStore, MockTransactionValidator, Pool,
};

use crate::{EthApiBuilder, RpcNodeCore, RpcNodeCoreAdapter};

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
