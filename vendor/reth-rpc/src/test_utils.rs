//! Base fixtures for the shared RPC implementation.

use core::convert::Infallible;

use alloy_consensus::transaction::TransactionInfo;
use alloy_evm::rpc::{EthTxEnvError, TryIntoTxEnv};
use alloy_rpc_types_eth::Log;
use base_common_consensus::{BaseReceipt, BaseTransactionInfo, BaseTxEnvelope};
use base_common_rpc_types::{BaseLogResponse, BaseTransactionReceipt, BaseTransactionRequest};
use base_execution_chainspec::ChainSpecProvider;
use base_execution_txpool::BasePooledTransaction;
use reth_evm::{EvmEnvFor, TestEvmConfig};
use reth_primitives_traits::SealedHeader;
use reth_rpc_convert::{
    RpcConverter, TxInfoMapper,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_api::{RpcNodeCore, node::RpcNodeCoreAdapter};
use reth_rpc_eth_types::{EthApiError, receipt::build_receipt};
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, blobstore::InMemoryBlobStore, noop::MockTransactionValidator,
};
use revm::context::TxEnv;

use crate::EthApiBuilder;

/// Pool accepting Base transactions in RPC tests.
pub type TestPool = Pool<
    MockTransactionValidator<BasePooledTransaction>,
    CoinbaseTipOrdering<BasePooledTransaction>,
    InMemoryBlobStore,
>;

/// Receipt converter for fixtures without L1 state or BaseTime metadata.
#[derive(Debug, Clone)]
pub struct TestReceiptConverter;

impl ReceiptConverter for TestReceiptConverter {
    type RpcReceipt = BaseTransactionReceipt;
    type RpcLog = BaseLogResponse;
    type Error = EthApiError;

    fn convert_log(
        &self,
        log: Log,
        _receipt: &BaseReceipt,
        _header: &SealedHeader,
    ) -> Result<BaseLogResponse, EthApiError> {
        Ok(log.into())
    }

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_>>,
    ) -> Result<Vec<BaseTransactionReceipt>, EthApiError> {
        Ok(inputs
            .into_iter()
            .map(|input| BaseTransactionReceipt {
                inner: build_receipt(input, None, |receipt, next_log_index, meta| {
                    let mut index = next_log_index;
                    receipt
                        .map_logs(|inner| {
                            let log_index = index as u64;
                            index += 1;
                            BaseLogResponse::from(Log {
                                inner,
                                block_hash: Some(meta.block_hash),
                                block_number: Some(meta.block_number),
                                block_timestamp: Some(meta.timestamp),
                                transaction_hash: Some(meta.tx_hash),
                                transaction_index: Some(meta.index),
                                log_index: Some(log_index),
                                removed: false,
                            })
                        })
                        .into()
                }),
                l1_block_info: Default::default(),
                payer: None,
                phase_statuses: None,
                metadata: None,
            })
            .collect())
    }
}

/// Transaction metadata for fixtures without deposit receipts or BaseTime metadata.
#[derive(Debug, Clone)]
pub struct TestTxInfoMapper;

impl TxInfoMapper<BaseTxEnvelope> for TestTxInfoMapper {
    type Out = BaseTransactionInfo;
    type Err = Infallible;

    fn try_map(
        &self,
        _tx: &BaseTxEnvelope,
        tx_info: TransactionInfo,
    ) -> Result<Self::Out, Self::Err> {
        Ok(BaseTransactionInfo::new(tx_info, Default::default()))
    }
}

/// Request conversion function used by the Ethereum interpreter fixture.
pub type TestTxEnvBuilder =
    fn(BaseTransactionRequest, &EvmEnvFor<TestEvmConfig>) -> Result<TxEnv, EthTxEnvError>;

/// Converter used to test the shared RPC handlers against Base transactions.
pub type TestRpcConverter = RpcConverter<
    TestEvmConfig,
    TestReceiptConverter,
    (),
    TestTxInfoMapper,
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
    pub fn converter() -> TestRpcConverter {
        let tx_env: TestTxEnvBuilder =
            |request, evm_env| request.as_ref().clone().try_into_tx_env(evm_env);
        RpcConverter::new(TestReceiptConverter)
            .with_mapper(TestTxInfoMapper)
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
        Provider: ChainSpecProvider,
        RpcNodeCoreAdapter<Provider, TestPool, Network, TestEvmConfig>:
            RpcNodeCore<Provider = Provider, Evm = TestEvmConfig>,
    {
        let converter = Self::converter();
        EthApiBuilder::new(provider, pool, network, evm).map_converter(|_| converter)
    }
}
