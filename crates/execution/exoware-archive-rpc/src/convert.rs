//! Builds Base `eth_` responses from verified archive data.
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
use base_common_chains::Upgrades;
use base_common_consensus::{
    BaseBlock, BasePrimitives, BaseReceipt, BaseTransactionInfo, DepositInfo, DepositReceiptExt,
};
use base_common_rpc_types::{
    BaseBlockResponse, BaseHeaderResponse, BaseLogResponse, BaseTransactionReceipt, Transaction,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::extract_l1_info;
use base_execution_rpc::{BaseReceiptBuilder, BaseTimeCache};
use base_exoware_archive::{ArchiveError, ArchivedBlock, ArchivedLog, Family};
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_rpc_convert::transaction::ConvertReceiptInput;

/// Converts archive blocks, receipts and logs into Base RPC responses.
#[derive(Debug, Clone)]
pub struct ArchiveConverter {
    /// Chain spec for L1 fee fields.
    pub chain_spec: Arc<BaseChainSpec>,
    /// `BaseTime` millisecond timestamps of archived blocks.
    pub base_time: BaseTimeCache,
}

/// One archived block, recovered and ready for conversion.
#[derive(Debug, Clone)]
pub struct PreparedBlock {
    /// Recovered block with the verified hash and senders.
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

    /// Recovers a verified archive block (hash and senders already checked) and resolves its
    /// `BaseTime` timestamp.
    pub fn prepare(&self, archived: ArchivedBlock) -> PreparedBlock {
        let ArchivedBlock { hash, header, body, receipts, senders } = archived;
        let block = BaseBlock {
            header,
            body: alloy_consensus::BlockBody {
                transactions: body.transactions,
                ommers: Vec::new(),
                withdrawals: body.withdrawals,
            },
        };
        let block = RecoveredBlock::new_sealed(SealedBlock::new_unchecked(block, hash), senders);
        let timestamp_ms = self.base_time.insert_from_transactions(
            hash,
            block.header().number,
            block.header().timestamp,
            &block.body().transactions,
        );
        PreparedBlock { block, receipts, timestamp_ms }
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

    /// Whether deposit fields that the fork at `timestamp` requires are absent: the deposit
    /// nonce from Regolith on, the deposit receipt version from Canyon on. The node fills both
    /// from the deposit's receipt and defaults them to `None` when the receipt is gone.
    pub fn deposit_fields_missing(
        &self,
        timestamp: u64,
        nonce: Option<u64>,
        version: Option<u64>,
    ) -> bool {
        (nonce.is_none() && self.chain_spec.is_regolith_active_at_timestamp(timestamp))
            || (version.is_none() && self.chain_spec.is_canyon_active_at_timestamp(timestamp))
    }

    /// Whether a mined deposit transaction response visibly lacks a deposit field its fork
    /// requires. `block_timestamp` is used when the response carries none; a pending or
    /// non-deposit transaction is complete.
    pub fn transaction_incomplete(&self, tx: &Transaction, block_timestamp: Option<u64>) -> bool {
        if !tx.inner.inner.inner().is_deposit() {
            return false;
        }
        tx.inner.block_timestamp.or(block_timestamp).is_some_and(|timestamp| {
            self.deposit_fields_missing(timestamp, tx.deposit_nonce, tx.deposit_receipt_version)
        })
    }

    /// Whether a full block response holds a deposit that visibly lacks a required field. A
    /// hashes-only block carries no deposit fields and is complete.
    pub fn block_incomplete(&self, block: &BaseBlockResponse) -> bool {
        let timestamp = Some(block.header.inner.timestamp);
        block
            .transactions
            .as_transactions()
            .is_some_and(|txs| txs.iter().any(|tx| self.transaction_incomplete(tx, timestamp)))
    }

    /// Deposit nonce and receipt version of a deposit receipt response, `None` for other
    /// receipts.
    pub const fn receipt_deposit_fields(
        receipt: &BaseTransactionReceipt,
    ) -> Option<(Option<u64>, Option<u64>)> {
        match &receipt.inner.inner.receipt {
            BaseReceipt::Deposit(deposit) => {
                Some((deposit.deposit_nonce, deposit.deposit_receipt_version))
            }
            _ => None,
        }
    }

    /// `eth_getBlockBy*` response.
    pub fn block(&self, prepared: &PreparedBlock, full: bool) -> BaseBlockResponse {
        let kind = if full { BlockTransactionsKind::Full } else { BlockTransactionsKind::Hashes };
        let timestamp_ms = prepared.timestamp_ms;
        let receipts = &prepared.receipts;
        let block: Result<BaseBlockResponse, std::convert::Infallible> =
            prepared.block.clone_into_rpc_block(
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

    /// Transaction response for transaction `index` of the block, `None` past the end.
    pub fn transaction(&self, prepared: &PreparedBlock, index: usize) -> Option<Transaction> {
        let block = &prepared.block;
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
        let deposit_meta = Self::deposit_meta(&prepared.receipts, Some(index as u64));
        Some(Transaction::from_transaction(
            tx,
            BaseTransactionInfo {
                inner: info,
                deposit_meta,
                block_timestamp_ms: prepared.timestamp_ms,
            },
        ))
    }

    /// Every receipt of the block, as `eth_getBlockReceipts` returns them.
    pub fn receipts(
        &self,
        prepared: &PreparedBlock,
    ) -> Result<Vec<BaseTransactionReceipt>, ArchiveError> {
        if prepared.receipts.is_empty() {
            return Ok(Vec::new());
        }
        (0..prepared.receipts.len())
            .map(|index| {
                self.receipt(prepared, index)?.ok_or_else(|| {
                    ArchiveError::unavailable(
                        Family::Receipts,
                        format!("receipt {index} has no transaction"),
                    )
                })
            })
            .collect()
    }

    /// `eth_getTransactionReceipt` response for transaction `index` of the block.
    pub fn receipt(
        &self,
        prepared: &PreparedBlock,
        index: usize,
    ) -> Result<Option<BaseTransactionReceipt>, ArchiveError> {
        let block = &prepared.block;
        if index >= prepared.receipts.len() {
            return Ok(None);
        }
        let mut l1_block_info = extract_l1_info(block.body()).map_err(|err| {
            ArchiveError::unavailable(Family::Body, format!("L1 block info: {err}"))
        })?;
        let header = block.header();
        let mut gas_before = 0;
        let mut logs_before = 0;
        for receipt in &prepared.receipts[..index] {
            gas_before = receipt.cumulative_gas_used();
            logs_before += receipt.logs().len();
        }
        let receipt = prepared.receipts[index].clone();
        let gas_used = receipt.cumulative_gas_used().saturating_sub(gas_before);
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
                .map_err(|err| {
                    ArchiveError::unavailable(Family::Receipts, format!("receipt fields: {err}"))
                })?
                .build();
        for log in &mut receipt.inner.inner.receipt.as_receipt_mut().logs {
            log.block_timestamp_ms = prepared.timestamp_ms;
        }
        Ok(Some(receipt))
    }

    /// `eth_getLogs` entry for one archived log.
    pub fn log(archived: &ArchivedLog) -> BaseLogResponse {
        BaseLogResponse {
            inner: Log {
                inner: archived.log.clone(),
                block_hash: Some(archived.block_hash),
                block_number: Some(archived.block_number),
                block_timestamp: Some(archived.block_timestamp),
                transaction_hash: Some(archived.transaction_hash),
                transaction_index: Some(archived.transaction_index),
                log_index: Some(archived.log_index),
                removed: false,
            },
            block_timestamp_ms: archived.block_timestamp_ms,
        }
    }
}
