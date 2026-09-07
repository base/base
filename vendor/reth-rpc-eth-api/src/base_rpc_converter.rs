//! Concrete Base transaction, receipt, log, and header conversion.

use alloy_consensus::transaction::Recovered;
use alloy_rpc_types_eth::{Log, TransactionInfo};
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_common_rpc_types::{
    BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt, BaseTransactionRequest,
};
use base_execution_chainspec::ChainSpecProvider;
use reth_evm::{EvmEnvFor, TxEnvFor};
use reth_primitives_traits::SealedBlock;
use reth_rpc_convert::{
    FromConsensusHeader, FromConsensusTx, RpcConvert, TransactionConversionError, TryIntoSimTx,
    TryIntoTxEnv, TxInfoMapper,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_types::BaseEthApiError;
use reth_storage_api::BlockReader;

use crate::{BaseReceiptConverter, BaseTimeCache, BaseTxInfoMapper};

/// Converts Base RPC data using the provider and shared BaseTime cache.
#[derive(Clone)]
pub struct BaseRpcConverter<Provider> {
    /// Receipt and L1 fee conversion.
    pub receipt_converter: BaseReceiptConverter<Provider>,
    /// Deposit and block timestamp metadata.
    pub mapper: BaseTxInfoMapper<Provider>,
}

impl<Provider> std::fmt::Debug for BaseRpcConverter<Provider> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseRpcConverter").finish_non_exhaustive()
    }
}

impl<Provider: Clone> BaseRpcConverter<Provider> {
    /// Creates conversion state shared by all Base RPC endpoints.
    pub fn new(provider: Provider, base_time: BaseTimeCache) -> Self {
        Self {
            receipt_converter: BaseReceiptConverter::new(provider.clone(), base_time.clone()),
            mapper: BaseTxInfoMapper::new(provider, base_time),
        }
    }
}

impl<Provider> RpcConvert for BaseRpcConverter<Provider>
where
    Provider: BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope, Receipt = BaseReceipt>
        + ChainSpecProvider
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
{
    type Error = BaseEthApiError;

    fn fill(
        &self,
        tx: Recovered<BaseTxEnvelope>,
        tx_info: TransactionInfo,
    ) -> Result<base_common_rpc_types::Transaction, Self::Error> {
        let (tx, signer) = tx.into_parts();
        let tx_info = self.mapper.try_map(&tx, tx_info)?;

        base_common_rpc_types::Transaction::from_consensus_tx(tx, signer, tx_info)
            .map_err(Into::into)
    }

    fn build_simulate_v1_transaction(
        &self,
        request: BaseTransactionRequest,
    ) -> Result<BaseTxEnvelope, Self::Error> {
        Ok(request
            .try_into_sim_tx()
            .map_err(|e| TransactionConversionError::FromTxReq(e.to_string()))?)
    }

    fn tx_env(
        &self,
        request: BaseTransactionRequest,
        evm_env: &EvmEnvFor,
    ) -> Result<TxEnvFor, Self::Error> {
        request.try_into_tx_env(evm_env).map_err(Into::into)
    }

    fn convert_log(
        &self,
        log: Log,
        receipt: &BaseReceipt,
        header: &reth_primitives_traits::SealedHeader,
    ) -> Result<BaseLogResponse, Self::Error> {
        self.receipt_converter.convert_log(log, receipt, header)
    }

    fn convert_receipts(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
    ) -> Result<Vec<BaseTransactionReceipt>, Self::Error> {
        self.receipt_converter.convert_receipts(receipts)
    }

    fn convert_receipts_with_block(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
        block: &SealedBlock<BaseBlock>,
    ) -> Result<Vec<BaseTransactionReceipt>, Self::Error> {
        self.receipt_converter.convert_receipts_with_block(receipts, block)
    }

    fn convert_header(
        &self,
        header: reth_primitives_traits::SealedHeader,
        block_size: usize,
    ) -> Result<BaseHeaderResponse, Self::Error> {
        Ok(BaseHeaderResponse::from_consensus_header(header, block_size))
    }
}
