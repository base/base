//! Base fixtures for the shared RPC implementation.

use std::sync::Arc;

use alloy_consensus::ReceiptWithBloom;
use alloy_evm::rpc::{EthTxEnvError, TryIntoTxEnv};
use alloy_rpc_types_eth::{Header, Log, Transaction, TransactionReceipt};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::BasePooledTransaction;
use reth_chainspec::{ChainSpec, ChainSpecProvider};
use reth_evm::{EvmEnvFor, TestEvmConfig};
use reth_primitives_traits::TransactionMeta;
use reth_rpc_convert::{RpcConverter, RpcTypes};
use reth_rpc_eth_api::{RpcNodeCore, node::RpcNodeCoreAdapter};
use reth_rpc_eth_types::receipt::EthReceiptConverter;
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, blobstore::InMemoryBlobStore, noop::MockTransactionValidator,
};
use revm::context::TxEnv;

use crate::EthApiBuilder;

/// RPC shapes for testing shared handlers with Base consensus primitives.
#[derive(Clone, Copy, Debug)]
pub struct TestRpcTypes;

impl RpcTypes for TestRpcTypes {
    type Header = Header;
    type Receipt = TransactionReceipt<ReceiptWithBloom<BaseReceipt<Log>>>;
    type Log = Log;
    type TransactionResponse = Transaction<BaseTxEnvelope>;
    type TransactionRequest = BaseTransactionRequest;
}

/// Pool accepting Base transactions in RPC tests.
pub type TestPool = Pool<
    MockTransactionValidator<BasePooledTransaction>,
    CoinbaseTipOrdering<BasePooledTransaction>,
    InMemoryBlobStore,
>;

/// Receipt conversion function used by RPC fixtures.
pub type TestReceiptBuilder =
    fn(BaseReceipt, usize, TransactionMeta) -> ReceiptWithBloom<BaseReceipt<Log>>;

/// Request conversion function used by the Ethereum interpreter fixture.
pub type TestTxEnvBuilder =
    fn(BaseTransactionRequest, &EvmEnvFor<TestEvmConfig>) -> Result<TxEnv, EthTxEnvError>;

/// Converter used to test the shared RPC handlers against Base transactions.
pub type TestRpcConverter = RpcConverter<
    TestRpcTypes,
    TestEvmConfig,
    EthReceiptConverter<ChainSpec, TestReceiptBuilder>,
    (),
    (),
    (),
    (),
    TestTxEnvBuilder,
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

    /// Creates a converter preserving transaction types and receipt log metadata.
    pub fn converter(chain_spec: Arc<ChainSpec>) -> TestRpcConverter {
        let receipt_builder: TestReceiptBuilder = |receipt, next_log_index, meta| {
            let mut index = next_log_index;
            receipt
                .map_logs(|inner| {
                    let log_index = index as u64;
                    index += 1;
                    Log {
                        inner,
                        block_hash: Some(meta.block_hash),
                        block_number: Some(meta.block_number),
                        block_timestamp: Some(meta.timestamp),
                        transaction_hash: Some(meta.tx_hash),
                        transaction_index: Some(meta.index),
                        log_index: Some(log_index),
                        removed: false,
                    }
                })
                .into()
        };
        let tx_env: TestTxEnvBuilder =
            |request, evm_env| request.as_ref().clone().try_into_tx_env(evm_env);
        RpcConverter::new(EthReceiptConverter::new(chain_spec).with_builder(receipt_builder))
            .with_tx_env_converter(tx_env)
    }

    /// Builds shared RPC handlers with the supplied provider and Base fixtures.
    pub fn api_builder<Provider, Network>(
        provider: Provider,
        pool: TestPool,
        network: Network,
        evm: TestEvmConfig,
    ) -> EthApiBuilder<
        RpcNodeCoreAdapter<Provider, TestPool, Network, TestEvmConfig>,
        TestRpcConverter,
    >
    where
        Provider: ChainSpecProvider<ChainSpec = ChainSpec>,
        RpcNodeCoreAdapter<Provider, TestPool, Network, TestEvmConfig>:
            RpcNodeCore<Provider = Provider, Evm = TestEvmConfig>,
    {
        let converter = Self::converter(provider.chain_spec());
        EthApiBuilder::new(provider, pool, network, evm).map_converter(|_| converter)
    }
}
