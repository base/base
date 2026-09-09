//! RPC receipt response builder, extends a layer one receipt with layer two data.

use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{Address, TxKind};
use base_common_types_chain::{BaseReceipt, Transaction};
use base_common_types_rpc::TransactionReceipt;
use reth_primitives_traits::TransactionMeta;
use reth_rpc_convert::transaction::ConvertReceiptInput;

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
