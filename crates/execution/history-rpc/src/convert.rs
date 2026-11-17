//! Builds Base `eth_` responses from archive data.
//!
//! The node's own converters read the node's database for side data (deposit receipts,
//! `BaseTime` transactions, the block for L1 fee fields), which a pruned node no longer has.
//! These builders take the same inputs from the archive and call the same Base response
//! constructors, so archive answers match what the node served before it pruned.

use std::sync::Arc;

use alloy_consensus::{
    TxReceipt,
    transaction::{TransactionInfo, TransactionMeta},
};
use alloy_primitives::U256;
use alloy_rpc_types_eth::{BlockTransactionsKind, Header, Log};
use base_common_consensus::{
    BaseBlock, BasePrimitives, BaseReceipt, BaseTransactionInfo, DepositInfo, DepositReceiptExt,
};
use base_common_rpc_types::{
    BaseBlockResponse, BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt, Transaction,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::extract_l1_info;
use base_execution_rpc::{BaseReceiptBuilder, BaseTimeCache};
use base_history_store::{ArchivedBlock, ArchivedLog, HistoryStoreError};
use reth_primitives_traits::RecoveredBlock;
use reth_rpc_eth_api::transaction::ConvertReceiptInput;

/// Converts archive blocks, receipts and logs into Base RPC responses.
#[derive(Debug, Clone)]
pub struct ArchiveConverter {
    /// Chain spec for L1 fee fields.
    pub chain_spec: Arc<BaseChainSpec>,
    /// `BaseTime` millisecond timestamps of archived blocks.
    pub base_time: BaseTimeCache,
}

/// One archived block with its receipts, ready for conversion.
#[derive(Debug, Clone)]
pub struct ArchivedBlockWithReceipts {
    /// Recovered block.
    pub block: RecoveredBlock<BaseBlock>,
    /// Receipts in transaction order.
    pub receipts: Vec<BaseReceipt>,
    /// `BaseTime` millisecond timestamp, when the block carries one.
    pub timestamp_ms: Option<u64>,
}

impl ArchiveConverter {
    /// Creates a converter.
    pub fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec, base_time: BaseTimeCache::default() }
    }

    /// Recovers an archived block and resolves its `BaseTime` timestamp.
    pub fn prepare(
        &self,
        block: ArchivedBlock,
        receipts: Vec<BaseReceipt>,
    ) -> ArchivedBlockWithReceipts {
        let block = block.into_recovered();
        let timestamp_ms = self.base_time.insert_from_transactions(
            block.hash(),
            block.header().number,
            block.header().timestamp,
            &block.body().transactions,
        );
        ArchivedBlockWithReceipts { block, receipts, timestamp_ms }
    }

    /// `BaseTime` millisecond timestamp of an archived block, when it carries one.
    pub fn timestamp_ms(&self, block: &ArchivedBlock) -> Option<u64> {
        self.base_time.insert_from_transactions(
            block.hash,
            block.header.number,
            block.header.timestamp,
            &block.transactions,
        )
    }

    /// Deposit metadata of transaction `index`, read from its archived receipt.
    pub fn deposit_meta(receipts: &[BaseReceipt], index: Option<u64>) -> DepositInfo {
        index
            .and_then(|index| receipts.get(index as usize))
            .and_then(DepositReceiptExt::as_deposit_receipt)
            .map(|receipt| DepositInfo {
                deposit_nonce: receipt.deposit_nonce,
                deposit_receipt_version: receipt.deposit_receipt_version,
            })
            .unwrap_or_default()
    }

    /// `eth_getBlockBy*` response.
    pub fn block(&self, archived: &ArchivedBlockWithReceipts, full: bool) -> BaseBlockResponse {
        let kind = if full { BlockTransactionsKind::Full } else { BlockTransactionsKind::Hashes };
        let timestamp_ms = archived.timestamp_ms;
        let receipts = &archived.receipts;
        let block: Result<BaseBlockResponse, std::convert::Infallible> =
            archived.block.clone_into_rpc_block(
                kind,
                |tx, info| {
                    let deposit_meta = Self::deposit_meta(receipts, info.index);
                    Ok(Transaction::from_transaction(
                        tx,
                        BaseTransactionInfo {
                            inner: info,
                            deposit_meta,
                            block_timestamp_ms: timestamp_ms,
                        },
                    ))
                },
                |header, size| {
                    Ok(BaseHeaderResponse::with_timestamp_ms(
                        Header::from_consensus(header.into(), None, Some(U256::from(size))),
                        timestamp_ms,
                    ))
                },
            );
        match block {
            Ok(block) => block,
            Err(never) => match never {},
        }
    }

    /// `eth_getTransactionByHash` response for transaction `index` of the block.
    pub fn transaction(
        &self,
        archived: &ArchivedBlockWithReceipts,
        index: usize,
    ) -> Option<Transaction> {
        let block = &archived.block;
        let tx = block.transactions_recovered().nth(index)?.cloned();
        let header = block.header();
        let info = TransactionInfo {
            hash: Some(tx.inner().tx_hash()),
            index: Some(index as u64),
            block_hash: Some(block.hash()),
            block_number: Some(header.number),
            base_fee: header.base_fee_per_gas,
            block_timestamp: Some(header.timestamp),
        };
        let deposit_meta = Self::deposit_meta(&archived.receipts, Some(index as u64));
        Some(Transaction::from_transaction(
            tx,
            BaseTransactionInfo {
                inner: info,
                deposit_meta,
                block_timestamp_ms: archived.timestamp_ms,
            },
        ))
    }

    /// `eth_getTransactionReceipt` response for transaction `index` of the block.
    pub fn receipt(
        &self,
        archived: &ArchivedBlockWithReceipts,
        index: usize,
    ) -> Result<Option<BaseTransactionReceipt>, HistoryStoreError> {
        let block = &archived.block;
        if index >= archived.receipts.len() {
            return Ok(None);
        }
        let mut l1_block_info = extract_l1_info(block.body()).map_err(|err| {
            HistoryStoreError::Unavailable { reason: format!("L1 block info: {err}") }
        })?;
        let header = block.header();
        let mut gas_before = 0;
        let mut logs_before = 0;
        for receipt in &archived.receipts[..index] {
            gas_before = receipt.cumulative_gas_used();
            logs_before += receipt.logs().len();
        }
        let receipt = archived.receipts[index].clone();
        let gas_used = receipt.cumulative_gas_used() - gas_before;
        let Some(tx) = block.transactions_recovered().nth(index) else { return Ok(None) };
        let meta = TransactionMeta {
            tx_hash: tx.inner().tx_hash(),
            index: index as u64,
            block_hash: block.hash(),
            block_number: header.number,
            base_fee: header.base_fee_per_gas,
            excess_blob_gas: header.excess_blob_gas,
            timestamp: header.timestamp,
        };
        let input = ConvertReceiptInput::<BasePrimitives> {
            receipt,
            tx,
            gas_used,
            next_log_index: logs_before,
            meta,
        };
        let mut receipt =
            BaseReceiptBuilder::new(self.chain_spec.as_ref(), input, &mut l1_block_info)
                .map_err(|err| HistoryStoreError::Unavailable {
                    reason: format!("receipt fields: {err}"),
                })?
                .build();
        for log in &mut receipt.inner.inner.receipt.as_receipt_mut().logs {
            log.block_timestamp_ms = archived.timestamp_ms;
        }
        Ok(Some(receipt))
    }

    /// `eth_getLogs` entry for one archived log.
    pub fn log(
        archived: &ArchivedLog,
        block_timestamp: u64,
        timestamp_ms: Option<u64>,
    ) -> BaseLogResponse {
        BaseLogResponse {
            inner: Log {
                inner: archived.log.clone(),
                block_hash: Some(archived.block_hash),
                block_number: Some(archived.block_number),
                block_timestamp: Some(block_timestamp),
                transaction_hash: Some(archived.tx_hash),
                transaction_index: Some(archived.tx_index),
                log_index: Some(archived.log_index),
                removed: false,
            },
            block_timestamp_ms: timestamp_ms,
        }
    }
}
