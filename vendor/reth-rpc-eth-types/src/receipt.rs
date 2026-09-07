//! RPC receipt response builder, extends a layer one receipt with layer two data.

use std::sync::Arc;

use alloy_consensus::{ReceiptEnvelope, Transaction};
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{Address, TxKind};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use base_common_consensus::BaseReceipt;
use base_execution_chainspec::BaseChainSpec;
use reth_ethereum_primitives::Receipt;
use reth_primitives_traits::TransactionMeta;
use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};

use crate::EthApiError;

/// Builds an [`TransactionReceipt`] obtaining the inner receipt envelope from the given closure.
pub fn build_receipt<E>(
    input: ConvertReceiptInput<'_>,
    blob_params: Option<BlobParams>,
    build_rpc_receipt: impl FnOnce(BaseReceipt, usize, TransactionMeta) -> E,
) -> TransactionReceipt<E> {
    let ConvertReceiptInput { tx, meta, receipt, gas_used, next_log_index } = input;
    let from = tx.signer();

    let blob_gas_used = tx.blob_gas_used();
    // Blob gas price should only be present if the transaction is a blob transaction
    let blob_gas_price =
        blob_gas_used.and_then(|_| Some(blob_params?.calc_blob_fee(meta.excess_blob_gas?)));

    let (contract_address, to) = match tx.kind() {
        TxKind::Create => (Some(from.create(tx.nonce())), None),
        TxKind::Call(addr) => (None, Some(Address(*addr))),
    };

    TransactionReceipt {
        inner: build_rpc_receipt(receipt, next_log_index, meta),
        transaction_hash: meta.tx_hash,
        transaction_index: Some(meta.index),
        block_hash: Some(meta.block_hash),
        block_number: Some(meta.block_number),
        from,
        to,
        gas_used,
        contract_address,
        effective_gas_price: tx.effective_gas_price(meta.base_fee),
        // EIP-4844 fields
        blob_gas_price,
        blob_gas_used,
    }
}

/// Converter for Ethereum receipts.
#[derive(derive_more::Debug)]
pub struct EthReceiptConverter<
    Builder = fn(Receipt, usize, TransactionMeta) -> ReceiptEnvelope<Log>,
> {
    chain_spec: Arc<BaseChainSpec>,
    #[debug(skip)]
    build_rpc_receipt: Builder,
}

impl<Builder> Clone for EthReceiptConverter<Builder>
where
    Builder: Clone,
{
    fn clone(&self) -> Self {
        Self {
            chain_spec: self.chain_spec.clone(),
            build_rpc_receipt: self.build_rpc_receipt.clone(),
        }
    }
}

impl EthReceiptConverter {
    /// Creates a new converter with the given chain spec.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self {
            chain_spec,
            build_rpc_receipt: |receipt: Receipt, next_log_index, meta: TransactionMeta| {
                let mut log_index = next_log_index;
                receipt
                    .map_logs(|log| {
                        let idx = log_index;
                        log_index += 1;
                        Log {
                            inner: log,
                            block_hash: Some(meta.block_hash),
                            block_number: Some(meta.block_number),
                            block_timestamp: Some(meta.timestamp),
                            transaction_hash: Some(meta.tx_hash),
                            transaction_index: Some(meta.index),
                            log_index: Some(idx as u64),
                            removed: false,
                        }
                    })
                    .into()
            },
        }
    }

    /// Sets new builder for the converter.
    pub fn with_builder<Builder>(self, build_rpc_receipt: Builder) -> EthReceiptConverter<Builder> {
        EthReceiptConverter { chain_spec: self.chain_spec, build_rpc_receipt }
    }
}

impl<Builder, Rpc> ReceiptConverter for EthReceiptConverter<Builder>
where
    Builder: Fn(BaseReceipt, usize, TransactionMeta) -> Rpc + 'static,
{
    type RpcReceipt = TransactionReceipt<Rpc>;
    type RpcLog = Log;
    type Error = EthApiError;

    fn convert_log(
        &self,
        log: Log,
        _receipt: &BaseReceipt,
        _header: &reth_primitives_traits::SealedHeader,
    ) -> Result<Self::RpcLog, Self::Error> {
        Ok(log)
    }

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let mut receipts = Vec::with_capacity(inputs.len());
        let blob_params = inputs
            .first()
            .and_then(|input| self.chain_spec.blob_params_at_timestamp(input.meta.timestamp));

        for input in inputs {
            receipts.push(build_receipt(input, blob_params, &self.build_rpc_receipt));
        }

        Ok(receipts)
    }
}
