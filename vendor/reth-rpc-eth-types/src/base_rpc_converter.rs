//! Concrete Base transaction, receipt, log, and header conversion.

use alloy_primitives::{Signature, U256};
use alloy_rpc_types_eth::{Log, TransactionInfo};
use base_common_consensus::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, SignableTransaction, error::ValueError,
    transaction::Recovered,
};
use base_common_rpc_types::{
    BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt, BaseTransactionRequest,
};
use base_execution_chainspec::ChainSpecProvider;
use base_execution_evm::{EvmEnvFor, TxEnvFor};
use reth_primitives_traits::SealedBlock;
use reth_rpc_convert::{
    TransactionConversionError, TryIntoTxEnv, transaction::ConvertReceiptInput,
};
use reth_storage_api::BlockReader;

use crate::{BaseEthApiError, BaseReceiptConverter, BaseTimeCache, BaseTxInfoMapper};

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

impl<Provider> BaseRpcConverter<Provider>
where
    Provider: BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope, Receipt = BaseReceipt>
        + ChainSpecProvider
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
{
    /// Converts a pending transaction without mined block metadata.
    pub fn fill_pending(
        &self,
        tx: Recovered<BaseTxEnvelope>,
    ) -> Result<base_common_rpc_types::Transaction, BaseEthApiError> {
        self.fill(tx, TransactionInfo::default())
    }

    /// Converts a mined transaction with its deposit and block metadata.
    pub fn fill(
        &self,
        tx: Recovered<BaseTxEnvelope>,
        tx_info: TransactionInfo,
    ) -> Result<base_common_rpc_types::Transaction, BaseEthApiError> {
        let (tx, signer) = tx.into_parts();
        let tx_info = self.mapper.try_map(&tx, tx_info)?;

        Ok(base_common_rpc_types::Transaction::from_transaction(
            Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }

    /// Builds the Base transaction used by eth_simulateV1.
    pub fn build_simulate_v1_transaction(
        &self,
        request: BaseTransactionRequest,
    ) -> Result<BaseTxEnvelope, BaseEthApiError> {
        let tx = request.build_typed_tx().map_err(|request| {
            TransactionConversionError::FromTxReq(
                ValueError::new(request, "Required fields missing").to_string(),
            )
        })?;
        Ok(tx.into_signed(Signature::new(U256::ZERO, U256::ZERO, false)).into())
    }

    /// Builds the Base execution environment for an RPC transaction request.
    pub fn tx_env(
        &self,
        request: BaseTransactionRequest,
        evm_env: &EvmEnvFor,
    ) -> Result<TxEnvFor, BaseEthApiError> {
        request.try_into_tx_env(evm_env).map_err(Into::into)
    }

    /// Adds Base block timestamp metadata to a log.
    pub fn convert_log(
        &self,
        log: Log,
        receipt: &BaseReceipt,
        header: &reth_primitives_traits::SealedHeader,
    ) -> Result<BaseLogResponse, BaseEthApiError> {
        self.receipt_converter.convert_log(log, receipt, header)
    }

    /// Converts receipts and calculates Base L1 fees.
    pub fn convert_receipts(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
    ) -> Result<Vec<BaseTransactionReceipt>, BaseEthApiError> {
        self.receipt_converter.convert_receipts(receipts)
    }

    /// Converts receipts using their loaded block.
    pub fn convert_receipts_with_block(
        &self,
        receipts: Vec<ConvertReceiptInput<'_>>,
        block: &SealedBlock,
    ) -> Result<Vec<BaseTransactionReceipt>, BaseEthApiError> {
        self.receipt_converter.convert_receipts_with_block(receipts, block)
    }

    /// Converts a Base consensus header to its RPC representation.
    pub fn convert_header(
        &self,
        header: reth_primitives_traits::SealedHeader,
        block_size: usize,
    ) -> Result<BaseHeaderResponse, BaseEthApiError> {
        Ok(BaseHeaderResponse::new(alloy_rpc_types_eth::Header::from_consensus(
            header.into(),
            None,
            Some(U256::from(block_size)),
        )))
    }
}
