use crate::pending::PendingBlock;
use crate::metrics::Metrics;
use crate::subscription::{Flashblock, Metadata};
use alloy_consensus::transaction::{
    Recovered, SignerRecoverable, TransactionInfo, TransactionMeta,
};
use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, Sealable, TxHash, B256, U256};
use alloy_provider::network::primitives::BlockTransactions;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use alloy_rpc_types_eth::Header;
use arc_swap::ArcSwap;
use op_alloy_consensus::{OpDepositReceipt, OpTxEnvelope};
use op_alloy_network::Optimism;
use op_alloy_rpc_types::Transaction;
use reth::api::BlockBody;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::{OpBlock, OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use rollup_boost::ExecutionPayloadBaseV1;
use serde::{de::DeserializeOwned, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::ops::Add;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info};

/// A receipt with its transaction hash for broadcasting
#[derive(Debug, Clone)]
pub struct ReceiptWithHash {
    pub tx_hash: TxHash,
    pub receipt: OpReceipt,
    pub block_number: u64,
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum CacheKey {
    Transaction(B256),                                        // tx_hash
    TransactionSender(B256),                                  // tx_sender:tx_hash
    TransactionBlockNumber(B256),                             // tx_block_number:tx_hash
    TransactionIndex(B256),                                   // tx_idx:tx_hash
    TransactionCount { address: Address, block_number: u64 }, // tx_count:from_address:block_number
    Receipt(B256),                                            // receipt:tx_hash
    ReceiptBlock(B256),                                       // receipt_block:tx_hash
    Block(u64),                                               // block:block_number
    Base(u64),                                                // base:block_number
    PendingBlock,                                             // pending
    PendingReceipts(u64),                                     // pending_receipts:block_number
    DiffTransactions(u64),                                    // diff:transactions:block_number
    HighestPayloadIndex,                                      // highest_payload_index
}

impl Display for CacheKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheKey::Transaction(hash) => write!(f, "{hash:?}"),
            CacheKey::TransactionSender(hash) => write!(f, "tx_sender:{hash:?}"),
            CacheKey::TransactionBlockNumber(hash) => write!(f, "tx_block_number:{hash:?}"),
            CacheKey::TransactionIndex(hash) => write!(f, "tx_idx:{hash:?}"),
            CacheKey::TransactionCount {
                address,
                block_number,
            } => {
                write!(f, "tx_count:{address}:{block_number}")
            }
            CacheKey::Receipt(hash) => write!(f, "receipt:{hash:?}"),
            CacheKey::ReceiptBlock(hash) => write!(f, "receipt_block:{hash:?}"),
            CacheKey::Block(number) => write!(f, "block:{number:?}"),
            CacheKey::Base(number) => write!(f, "base:{number:?}"),
            CacheKey::PendingBlock => write!(f, "pending"),
            CacheKey::PendingReceipts(number) => write!(f, "pending_receipts:{number:?}"),
            CacheKey::DiffTransactions(number) => write!(f, "diff:transactions:{number:?}"),
            CacheKey::HighestPayloadIndex => write!(f, "highest_payload_index"),
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    expiry: Instant,
}

#[derive(Debug, Clone)]
pub struct FlashblocksState {
    current_state: Arc<ArcSwap<PendingBlock>>,

    store: Arc<RwLock<HashMap<CacheKey, CacheEntry<Vec<u8>>>>>,
    receipt_sender: broadcast::Sender<ReceiptWithHash>,
    metrics: Metrics,
    chain_spec: Arc<OpChainSpec>,
}

impl FlashblocksState {
    pub fn new(chain_spec: Arc<OpChainSpec>, receipt_buffer_size: usize) -> Self {
        Self {
            chain_spec,
            current_state: Arc::new(ArcSwap::from_pointee(PendingBlock::empty())),
            store: Arc::new(RwLock::new(HashMap::new())),
            receipt_sender: broadcast::channel(receipt_buffer_size).0,
            metrics: Metrics::default(),
        }
    }

    pub fn block_by_number(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        self.get::<OpBlock>(&CacheKey::PendingBlock)
            .map(|block| self.transform_block(block, full))
    }

    pub fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        self.get::<OpReceipt>(&CacheKey::Receipt(tx_hash))
            .map(|receipt| {
                self.transform_receipt(
                    receipt,
                    tx_hash,
                    self.get::<u64>(&CacheKey::ReceiptBlock(tx_hash)).unwrap(),
                )
            })
    }

    pub fn get_transaction_count(&self, block_num: u64, address: Address) -> U256 {
        // self.current_state.load().get_transaction_count(address)
        U256::from(
            self.get::<u64>(&CacheKey::TransactionCount {
                address,
                block_number: block_num + 1,
            })
            .unwrap_or(0),
        )
    }

    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Optimism>> {
        // Handle cache lookup for transactions not found in the main lookup
        self.get::<OpTransactionSigned>(&CacheKey::Transaction(tx_hash))
            .map(|tx| {
                let block_number = self
                    .get::<u64>(&CacheKey::TransactionBlockNumber(tx_hash))
                    .unwrap();
                let block = self.get::<OpBlock>(&CacheKey::Block(block_number)).unwrap();
                let index = self
                    .get::<u64>(&CacheKey::TransactionIndex(tx_hash))
                    .unwrap();
                let tx_info = TransactionInfo {
                    hash: Some(tx.tx_hash()),
                    block_hash: Some(block.header.hash_slow()),
                    block_number: Some(block.number),
                    index: Some(index),
                    base_fee: block.base_fee_per_gas,
                };
                let sender = self
                    .get::<Address>(&CacheKey::TransactionSender(tx_hash))
                    .unwrap();
                let tx = self
                    .get::<OpTransactionSigned>(&CacheKey::Transaction(tx_hash))
                    .unwrap();
                let tx = Recovered::new_unchecked(tx, sender);
                self.transform_tx(tx, tx_info, None)
            })
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.current_state.load().get_balance(address)
    }

    pub fn subscribe_to_receipts(&self) -> broadcast::Receiver<ReceiptWithHash> {
        self.receipt_sender.subscribe()
    }

    pub fn cleanup_expired(&self) {
        if let Ok(mut store) = self.store.write() {
            store.retain(|_, entry| entry.expiry < Instant::now());
        }
    }

    // TODO: Refactor

    fn is_next_flashblock(&self, flashblock: &Flashblock) -> bool {
        flashblock.metadata.block_number == self.current_state.load().block_number
            && flashblock.index == self.current_state.load().index_number + 1
    }

    pub fn on_flashblock_received(&self, flashblock: Flashblock) {
        let current_state = self.current_state.load();

        // todo remove once method is simplified
        let flashblock_clone = flashblock.clone();

        if flashblock.index == 0 {
            self.current_state
                .swap(Arc::new(PendingBlock::new_block(flashblock_clone)));
        } else if self.is_next_flashblock(&flashblock) {
            self.current_state
                .swap(Arc::new(PendingBlock::extend_block(
                    &current_state,
                    flashblock_clone,
                )));
        } else if current_state.block_number != flashblock.metadata.block_number {
            info!(
                message = "Received Flashblock for new block, zero'ing Flashblocks until ",
                curr_block = %current_state.block_number,
                new_block = %flashblock.metadata.block_number,
            );

            self.current_state.swap(Arc::new(PendingBlock::empty()));
        } else {
            info!(
                message = "None sequential Flashblocks, keeping cache",
                curr_block = %current_state.block_number,
                new_block = %flashblock.metadata.block_number,
            );
        }

        let msg_processing_start_time = Instant::now();

        let block_number = flashblock.metadata.block_number;
        let diff = flashblock.diff;
        let withdrawals = diff.withdrawals.clone();
        let diff_transactions = diff.transactions.clone();
        let metadata = flashblock.metadata;

        // Skip if index is 0 and base is not cached, likely the first payload
        // Can't do pending block with this because already missing blocks
        if flashblock.index != 0
            && self
                .get::<ExecutionPayloadBaseV1>(&CacheKey::Base(block_number))
                .is_none()
        {
            return;
        }

        // Track flashblock indices and record metrics
        self.update_flashblocks_index(flashblock.index);

        // Prevent updating to older blocks
        let current_block = self.get::<OpBlock>(&CacheKey::PendingBlock);
        if current_block.is_some() && current_block.unwrap().number > block_number {
            return;
        }

        // base only appears once in the first payload index
        let base = if let Some(base) = flashblock.base {
            if let Err(e) = self.set(CacheKey::Base(block_number), &base) {
                error!(
                    message = "failed to set base in cache",
                    error = %e
                );
                return;
            }
            base
        } else {
            match self.get(&CacheKey::Base(block_number)) {
                Some(base) => base,
                None => {
                    error!(message = "failed to get base from cache");
                    return;
                }
            }
        };

        let transactions = match self.get_and_set_transactions(
            diff_transactions,
            flashblock.index,
            block_number,
        ) {
            Ok(txs) => txs,
            Err(e) => {
                error!(
                    message = "failed to get and set transactions",
                    error = %e
                );
                return;
            }
        };

        let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                withdrawals,
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: base.parent_hash,
                    fee_recipient: base.fee_recipient,
                    state_root: diff.state_root,
                    receipts_root: diff.receipts_root,
                    logs_bloom: diff.logs_bloom,
                    prev_randao: base.prev_randao,
                    block_number: base.block_number,
                    gas_limit: base.gas_limit,
                    gas_used: diff.gas_used,
                    timestamp: base.timestamp,
                    extra_data: base.extra_data,
                    base_fee_per_gas: base.base_fee_per_gas,
                    block_hash: diff.block_hash,
                    transactions,
                },
            },
        };

        let block: OpBlock = match execution_payload.try_into_block() {
            Ok(block) => block,
            Err(e) => {
                error!(
                    message = "failed to convert execution payload to block",
                    error = %e
                );
                return;
            }
        };

        // "pending" because users query the block using "pending" tag
        // This is an optimistic update will likely need to tweak in the future
        if let Err(e) = self.set(CacheKey::PendingBlock, &block) {
            error!(
                message = "failed to set pending block in cache",
                error = %e
            );
            return;
        }

        // set block to block number as well
        if let Err(e) = self.set(CacheKey::Block(block_number), &block) {
            error!(
                message = "failed to set block in cache",
                error = %e
            );
            return;
        }

        let diff_receipts = match self.get_and_set_txs_and_receipts(
            block.clone(),
            block_number,
            metadata.clone(),
        ) {
            Ok(receipts) => receipts,
            Err(e) => {
                error!(
                    message = "failed to get and set receipts",
                    error = %e
                );
                return;
            }
        };

        // update all receipts
        let _receipts = match self.get_and_set_all_receipts(
            flashblock.index,
            block_number,
            diff_receipts.clone(),
        ) {
            Ok(receipts) => receipts,
            Err(e) => {
                error!(
                    message = "failed to get and set all receipts",
                    error = %e
                );
                return;
            }
        };

        self.metrics
            .block_processing_duration
            .record(msg_processing_start_time.elapsed());

        for (tx_hash_str, receipt) in &metadata.receipts {
            if let Ok(tx_hash) = alloy_primitives::TxHash::from_str(tx_hash_str) {
                let receipt_with_hash = ReceiptWithHash {
                    tx_hash,
                    receipt: receipt.clone(),
                    block_number,
                };

                match self.receipt_sender.send(receipt_with_hash) {
                    Ok(subscriber_count) => {
                        debug!(
                            message = "broadcasted receipt",
                            tx_hash = %tx_hash,
                            subscriber_count = subscriber_count
                        );
                    }
                    Err(_) => {
                        debug!(
                            message = "no active subscribers for receipt broadcast",
                            tx_hash = %tx_hash
                        );
                    }
                }
            }
        }

        // check duration on the most heavy payload
        if flashblock.index == 0 {
            info!(
                message = "block processing completed",
                processing_time = ?msg_processing_start_time.elapsed()
            );
        }
    }

    fn get<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<T> {
        let store = self.store.read().unwrap();
        store.get(key).and_then(|entry| {
            if entry.expiry < Instant::now() {
                return None;
            }
            serde_json::from_slice(&entry.value).ok()
        })
    }

    fn get_and_set_all_receipts(
        &self,
        payload_index: u64,
        block_number: u64,
        diff_receipts: Vec<OpReceipt>,
    ) -> Result<Vec<OpReceipt>, Box<dyn std::error::Error>> {
        // update all receipts
        let receipts = if payload_index == 0 {
            // get receipts and sort by cumulative gas used
            diff_receipts
        } else {
            let existing =
                match self.get::<Vec<OpReceipt>>(&CacheKey::PendingReceipts(block_number)) {
                    Some(existing) => existing,
                    None => {
                        return Err("Failed to get pending receipts from cache".into());
                    }
                };
            existing
                .into_iter()
                .chain(diff_receipts.iter().cloned())
                .collect()
        };

        self.set(CacheKey::PendingReceipts(block_number), &receipts)?;

        Ok(receipts)
    }

    fn update_flashblocks_index(&self, index: u64) {
        if index == 0 {
            // Get the highest index from previous block
            if let Some(prev_highest_index) = self.get::<u64>(&CacheKey::HighestPayloadIndex) {
                // Record metric: total flash blocks = highest_index + 1 (since it's 0-indexed)
                self.metrics
                    .flashblocks_in_block
                    .record((prev_highest_index + 1) as f64);
                info!(
                    message = "previous block processed",
                    flash_blocks_count = prev_highest_index + 1
                );
            }

            // Reset the highest index to 0 for new block
            if let Err(e) = self.set(CacheKey::HighestPayloadIndex, &0u64) {
                error!(
                    message = "failed to reset highest flash index",
                    error = %e
                );
            }
        } else {
            // Update highest index if current index is higher
            let current_highest = self.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap_or(0);
            if index > current_highest {
                if let Err(e) = self.set(CacheKey::HighestPayloadIndex, &index) {
                    error!(
                        message = "failed to update highest flash index",
                        error = %e
                    );
                }
            }
        }
    }

    fn get_and_set_transactions(
        &self,
        transactions: Vec<Bytes>,
        payload_index: u64,
        block_number: u64,
    ) -> Result<Vec<Bytes>, Box<dyn std::error::Error>> {
        // update incremental transactions
        let transactions = if payload_index == 0 {
            transactions
        } else {
            let existing = match self.get::<Vec<Bytes>>(&CacheKey::DiffTransactions(block_number)) {
                Some(existing) => existing,
                None => return Err("Failed to get pending transactions from cache".into()),
            };
            existing
                .into_iter()
                .chain(transactions.iter().cloned())
                .collect()
        };

        self.set(CacheKey::DiffTransactions(block_number), &transactions)?;

        Ok(transactions)
    }

    fn get_and_set_txs_and_receipts(
        &self,
        block: OpBlock,
        block_number: u64,
        metadata: Metadata,
    ) -> Result<Vec<OpReceipt>, Box<dyn std::error::Error>> {
        let mut diff_receipts: Vec<OpReceipt> = vec![];
        // Store tx transaction signed
        for (idx, transaction) in block.body.transactions.iter().enumerate() {
            // check if exists, if not update
            let existing_tx =
                self.get::<OpTransactionSigned>(&CacheKey::Transaction(transaction.tx_hash()));
            if existing_tx.is_none() {
                if let Err(e) = self.set(CacheKey::Transaction(transaction.tx_hash()), &transaction)
                {
                    error!(
                        message = "failed to set transaction in cache",
                        error = %e
                    );
                    continue;
                }
                // update tx index
                if let Err(e) = self.set(CacheKey::TransactionIndex(transaction.tx_hash()), &idx) {
                    error!(
                        message = "failed to set transaction index in cache",
                        error = %e
                    );
                    continue;
                }

                // update tx count for each from address
                if let Ok(from) = transaction.recover_signer() {
                    // Get current tx count, default to 0 if not found
                    let current_count = self
                        .get::<u64>(&CacheKey::TransactionCount {
                            address: from,
                            block_number,
                        })
                        .unwrap_or(0);
                    // Increment tx count by 1
                    if let Err(e) = self.set(
                        CacheKey::TransactionCount {
                            address: from,
                            block_number,
                        },
                        &(current_count + 1),
                    ) {
                        error!(
                            message = "failed to set transaction count in cache",
                            error = %e
                        );
                    }

                    // also keep track of sender of each transaction
                    if let Err(e) =
                        self.set(CacheKey::TransactionSender(transaction.tx_hash()), &from)
                    {
                        error!(
                            message = "failed to set transaction sender in cache",
                            error = %e
                        );
                    }

                    // also keep track of the block number of each transaction
                    if let Err(e) = self.set(
                        CacheKey::TransactionBlockNumber(transaction.tx_hash()),
                        &block_number,
                    ) {
                        error!(
                            message = "failed to set transaction block number in cache",
                            error = %e
                        );
                    }
                }
            }

            // TODO: move this into the transaction check
            if metadata
                .receipts
                .contains_key(&transaction.tx_hash().to_string())
            {
                // find receipt in metadata and set it in cache
                let receipt = metadata
                    .receipts
                    .get(&transaction.tx_hash().to_string())
                    .unwrap();
                if let Err(e) = self.set(CacheKey::Receipt(transaction.tx_hash()), receipt) {
                    error!(
                        message = "failed to set receipt in cache",
                        error = %e
                    );
                    continue;
                }
                // map receipt's block number as well
                if let Err(e) =
                    self.set(CacheKey::ReceiptBlock(transaction.tx_hash()), &block_number)
                {
                    error!(
                        message = "failed to set receipt block in cache",
                        error = %e
                    );
                    continue;
                }

                diff_receipts.push(receipt.clone());
            }
        }

        Ok(diff_receipts)
    }

    fn set<T: Serialize>(
        &self,
        key: CacheKey,
        value: &T,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let serialized = serde_json::to_vec(value)?;
        let entry = CacheEntry {
            value: serialized,
            expiry: Instant::now().add(Duration::from_secs(10)),
        };

        let mut store = self.store.write().unwrap();
        store.insert(key, entry);
        Ok(())
    }

    pub fn transform_block(&self, block: OpBlock, full: bool) -> RpcBlock<Optimism> {
        let header: alloy_consensus::Header = block.header.clone();
        let transactions = block.body.transactions.to_vec();

        let transactions = if full {
            let transactions_with_senders = transactions
                .into_iter()
                .zip(block.body.recover_signers().unwrap());
            let converted_txs = transactions_with_senders
                .enumerate()
                .map(|(idx, (tx, sender))| {
                    let signed_tx_ec_recovered = Recovered::new_unchecked(tx.clone(), sender);
                    let tx_info = TransactionInfo {
                        hash: Some(tx.tx_hash()),
                        block_hash: Some(block.header.hash_slow()),
                        block_number: Some(block.number),
                        index: Some(idx as u64),
                        base_fee: block.base_fee_per_gas,
                    };
                    self.transform_tx(signed_tx_ec_recovered, tx_info, None)
                })
                .collect();
            BlockTransactions::Full(converted_txs)
        } else {
            let tx_hashes = transactions.into_iter().map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Optimism> {
            header: Header::from_consensus(header.seal_slow(), None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: None,
        }
    }

    pub fn transform_tx(
        &self,
        tx: Recovered<OpTransactionSigned>,
        tx_info: TransactionInfo,
        deposit_receipt: Option<OpDepositReceipt>,
    ) -> Transaction {
        let tx = tx.convert::<OpTxEnvelope>();
        let mut deposit_receipt_version = None;
        let mut deposit_nonce = None;

        if tx.is_deposit() {
            if let Some(receipt) = deposit_receipt {
                deposit_receipt_version = receipt.deposit_receipt_version;
                deposit_nonce = receipt.deposit_nonce;
            } else {
                let cached_receipt = self
                    .get::<OpReceipt>(&CacheKey::Receipt(tx_info.hash.unwrap()))
                    .unwrap();

                if let OpReceipt::Deposit(receipt) = cached_receipt {
                    deposit_receipt_version = receipt.deposit_receipt_version;
                    deposit_nonce = receipt.deposit_nonce;
                }
            }
        }

        let TransactionInfo {
            block_hash,
            block_number,
            index: transaction_index,
            base_fee,
            ..
        } = tx_info;

        let effective_gas_price = if tx.is_deposit() {
            // For deposits, we must always set the `gasPrice` field to 0 in rpc
            // deposit tx don't have a gas price field, but serde of `Transaction` will take care of
            // it
            0
        } else {
            base_fee
                .map(|base_fee| {
                    tx.effective_tip_per_gas(base_fee).unwrap_or_default() + base_fee as u128
                })
                .unwrap_or_else(|| tx.max_fee_per_gas())
        };

        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: tx,
                block_hash,
                block_number,
                transaction_index,
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
        }
    }

    pub fn transform_receipt(
        &self,
        receipt: OpReceipt,
        tx_hash: TxHash,
        block_number: u64,
    ) -> RpcReceipt<Optimism> {
        let block = self.get::<OpBlock>(&CacheKey::Block(block_number)).unwrap();
        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("failed to extract l1 info");

        let index = self
            .get::<u64>(&CacheKey::TransactionIndex(tx_hash))
            .unwrap();
        let meta = TransactionMeta {
            tx_hash,
            index,
            block_hash: block.header.hash_slow(),
            block_number: block.number,
            base_fee: block.base_fee_per_gas,
            excess_blob_gas: block.excess_blob_gas,
            timestamp: block.timestamp,
        };

        // get all receipts from cache too
        let all_receipts = self
            .get::<Vec<OpReceipt>>(&CacheKey::PendingReceipts(block_number))
            .unwrap();

        let sender = self
            .get::<Address>(&CacheKey::TransactionSender(tx_hash))
            .unwrap();
        let tx = self
            .get::<OpTransactionSigned>(&CacheKey::Transaction(tx_hash))
            .unwrap();
        let tx = Recovered::new_unchecked(tx, sender);

        let mut gas_used = 0;
        let mut next_log_index = 0;

        if meta.index > 0 {
            for receipt in all_receipts.iter().take(meta.index as usize) {
                gas_used = receipt.cumulative_gas_used();
                next_log_index += receipt.logs().len();
            }
        }

        let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
            receipt: Cow::Borrowed(&receipt),
            tx: Recovered::new_unchecked(&*tx, tx.signer()),
            gas_used: receipt.cumulative_gas_used() - gas_used,
            next_log_index,
            meta,
        };

        OpReceiptBuilder::new(self.chain_spec.as_ref(), input, &mut l1_block_info)
            .expect("failed to build receipt")
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Receipt, TxReceipt};
    use alloy_primitives::{Address, B256, U256};
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpBlock;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use rollup_boost::ExecutionPayloadFlashblockDeltaV1;
    use std::str::FromStr;

    fn new_state() -> FlashblocksState {
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet()
                .ecotone_activated()
                .build(),
        );

        FlashblocksState::new(chain_spec, 2000)
    }

    fn create_first_payload() -> Flashblock {
        // First payload (index 0) setup remains the same
        let base = ExecutionPayloadBaseV1 {
            parent_hash: Default::default(),
            parent_beacon_block_root: Default::default(),
            fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            block_number: 1,
            gas_limit: 1000000,
            timestamp: 1234567890,
            prev_randao: Default::default(),
            extra_data: Default::default(),
            base_fee_per_gas: U256::from(10000),
        };

        let delta = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![],
            withdrawals: vec![],
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            gas_used: 0,
            block_hash: Default::default(),
            withdrawals_root: Default::default(),
        };

        let metadata = Metadata {
            block_number: 1,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        Flashblock {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(base),
            diff: delta,
            metadata: metadata,
        }
    }

    // Create payload with specific index and block number
    fn create_payload_with_index(index: u64, block_number: u64) -> Flashblock {
        let base = if index == 0 {
            Some(ExecutionPayloadBaseV1 {
                parent_hash: Default::default(),
                parent_beacon_block_root: Default::default(),
                fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890")
                    .unwrap(),
                block_number,
                gas_limit: 1000000,
                timestamp: 1234567890,
                prev_randao: Default::default(),
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(1000),
            })
        } else {
            None
        };

        let delta = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![],
            withdrawals: vec![],
            state_root: B256::repeat_byte(index as u8),
            receipts_root: B256::repeat_byte((index + 1) as u8),
            logs_bloom: Default::default(),
            gas_used: 21000 * index,
            block_hash: B256::repeat_byte((index + 2) as u8),
            withdrawals_root: Default::default(),
        };

        let metadata = Metadata {
            block_number,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        Flashblock {
            index,
            payload_id: PayloadId::new([0; 8]),
            base,
            diff: delta,
            metadata,
        }
    }

    fn create_second_payload() -> Flashblock {
        // Create second payload (index 1) with transactions
        // tx1 hash: 0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c
        // tx2 hash: 0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8
        let tx1 = Bytes::from_str("0x02f87483014a3482017e8459682f0084596830a98301f1d094b01866f195533de16eb929b73f87280693ca0cb480844e71d92dc001a0a658c18bdba29dd4022ee6640fdd143691230c12b3c8c86cf5c1a1f1682cc1e2a0248a28763541ebed2b87ecea63a7024b5c2b7de58539fa64c887b08f5faf29c1").unwrap();
        let tx2 = Bytes::from_str("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14").unwrap();

        let delta2 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx1.clone(), tx2.clone()],
            withdrawals: vec![],
            state_root: B256::repeat_byte(0x1),
            receipts_root: B256::repeat_byte(0x2),
            logs_bloom: Default::default(),
            gas_used: 21000,
            block_hash: B256::repeat_byte(0x3),
            withdrawals_root: Default::default(),
        };

        let metadata2 = Metadata {
            block_number: 1,
            receipts: {
                let mut receipts = HashMap::default();
                receipts.insert(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c"
                        .to_string(), // transaction hash as string
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8"
                        .to_string(), // transaction hash as string
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 42000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: {
                let mut map = HashMap::default();
                map.insert(
                    Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
                    U256::from_str("0x1234").unwrap(),
                );
                map
            },
        };

        Flashblock {
            index: 1,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta2,
            metadata: metadata2,
        }
    }

    #[test]
    fn test_process_payload() {
        let cache = new_state();
        let mut receipt_receiver = cache.subscribe_to_receipts();

        let payload = create_first_payload();

        // Process first payload
        cache.on_flashblock_received(payload);

        let payload2 = create_second_payload();
        // Process second payload
        cache.on_flashblock_received(payload2);

        // Check that receipts were broadcast for both transactions
        let mut receipts = vec![];
        receipts.push(receipt_receiver.try_recv().unwrap());
        receipts.push(receipt_receiver.try_recv().unwrap());

        // Sort receipts by tx_hash to ensure deterministic testing
        receipts.sort_by_key(|r| r.tx_hash);

        // These are defined in the second payload method
        let expected_tx1_hash =
            B256::from_str("0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c")
                .unwrap();
        let expected_tx2_hash =
            B256::from_str("0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8")
                .unwrap();

        assert_eq!(receipts[0].tx_hash, expected_tx1_hash);
        assert_eq!(receipts[0].block_number, 1);
        assert_eq!(receipts[0].receipt.cumulative_gas_used(), 21000);

        assert_eq!(receipts[1].tx_hash, expected_tx2_hash);
        assert_eq!(receipts[1].block_number, 1);
        assert_eq!(receipts[1].receipt.cumulative_gas_used(), 42000);

        // Verify no more receipts are available
        assert!(receipt_receiver.try_recv().is_err());

        // Verify final state
        let final_block = cache.get::<OpBlock>(&CacheKey::PendingBlock).unwrap();
        assert_eq!(final_block.body.transactions.len(), 2);
        assert_eq!(final_block.header.state_root, B256::repeat_byte(0x1));
        assert_eq!(final_block.header.receipts_root, B256::repeat_byte(0x2));
        assert_eq!(final_block.header.gas_used, 21000);
        assert_eq!(final_block.header.base_fee_per_gas, Some(10000));

        let tx1_receipt = cache
            .get::<OpReceipt>(&CacheKey::Receipt(
                B256::from_str(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx1_receipt.cumulative_gas_used(), 21000);

        let tx2_receipt = cache
            .get::<OpReceipt>(&CacheKey::Receipt(
                B256::from_str(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx2_receipt.cumulative_gas_used(), 42000);

        // verify tx_sender, tx_block_number, tx_idx
        let tx_sender = cache
            .get::<Address>(&CacheKey::TransactionSender(
                B256::from_str(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(
            tx_sender,
            Address::from_str("0xb63d5fd2e6c53fe06680c47736aba771211105e4").unwrap()
        );

        let tx_block_number = cache
            .get::<u64>(&CacheKey::TransactionBlockNumber(
                B256::from_str(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx_block_number, 1);

        let tx_idx = cache
            .get::<u64>(&CacheKey::TransactionIndex(
                B256::from_str(
                    "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx_idx, 0);

        let tx_sender2 = cache
            .get::<Address>(&CacheKey::TransactionSender(
                B256::from_str(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(
            tx_sender2,
            Address::from_str("0x6e5e56b972374e4fde8390df0033397df931a49d").unwrap()
        );

        let tx_block_number2 = cache
            .get::<u64>(&CacheKey::TransactionBlockNumber(
                B256::from_str(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx_block_number2, 1);

        let tx_idx2 = cache
            .get::<u64>(&CacheKey::TransactionIndex(
                B256::from_str(
                    "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
                )
                .unwrap(),
            ))
            .unwrap();
        assert_eq!(tx_idx2, 1);
    }

    #[test]
    fn test_skip_initial_non_zero_index_payload() {
        let cache = new_state();
        let metadata = Metadata {
            block_number: 1,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        let payload = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1, // Non-zero index but no base in cache
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata,
        };

        // Process payload
        cache.on_flashblock_received(payload);

        // Verify no block was stored, since it skips the first payload
        assert!(cache.get::<OpBlock>(&CacheKey::PendingBlock).is_none());
    }

    #[test]
    fn test_flash_block_tracking() {
        let cache = new_state();
        // Process first block with 3 flash blocks
        // Block 1, payload 0 (starts a new block)
        let payload1_0 = create_payload_with_index(0, 1);
        cache.on_flashblock_received(payload1_0);

        // Check that highest_payload_index was set to 0
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);

        // Block 1, payload 1
        let payload1_1 = create_payload_with_index(1, 1);
        cache.on_flashblock_received(payload1_1);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 1);

        // Block 1, payload 2
        let payload1_2 = create_payload_with_index(2, 1);
        cache.on_flashblock_received(payload1_2);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 2);

        // Now start a new block (block 2, payload 0)
        let payload2_0 = create_payload_with_index(0, 2);
        cache.on_flashblock_received(payload2_0);

        // Check that highest_payload_index was reset to 0
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);

        // Block 2, payload 1 (out of order with payload 3)
        let payload2_1 = create_payload_with_index(1, 2);
        cache.on_flashblock_received(payload2_1);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 1);

        // Block 2, payload 3 (skipping 2)
        let payload2_3 = create_payload_with_index(3, 2);
        cache.on_flashblock_received(payload2_3);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 3);

        // Block 2, payload 2 (out of order, should not change highest)
        let payload2_2 = create_payload_with_index(2, 2);
        cache.on_flashblock_received(payload2_2);

        // Check that highest_payload_index is still 3
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 3);

        // Start block 3, payload 0
        let payload3_0 = create_payload_with_index(0, 3);
        cache.on_flashblock_received(payload3_0);

        // Check that highest_payload_index was reset to 0
        // Also verify metric would have been recorded (though we can't directly check the metric's value)
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);
    }
}
