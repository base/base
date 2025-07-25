use crate::flashblocks::Metadata;
use crate::metrics::Metrics;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, Bytes, B256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use rollup_boost::primitives::{ExecutionPayloadBaseV1, FlashblocksPayloadV1};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info};

/// API trait for Flashblocks client functionality
pub trait FlashblocksApi {
    /// Subscribe to real-time receipt broadcasts
    fn subscribe_to_receipts(&self) -> broadcast::Receiver<ReceiptWithHash>;
}

/// A receipt with its transaction hash for broadcasting
#[derive(Debug, Clone)]
pub struct ReceiptWithHash {
    pub tx_hash: alloy_primitives::TxHash,
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
    AccountBalance(Address),                                  // address
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
            CacheKey::AccountBalance(addr) => write!(f, "{addr:?}"),
            CacheKey::HighestPayloadIndex => write!(f, "highest_payload_index"),
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    expiry: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    store: Arc<RwLock<HashMap<CacheKey, CacheEntry<Vec<u8>>>>>,
    receipt_sender: broadcast::Sender<ReceiptWithHash>,
    metrics: Metrics,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
            receipt_sender: broadcast::channel(2000).0,
            metrics: Metrics::default(),
        }
    }
}

impl FlashblocksApi for Cache {
    fn subscribe_to_receipts(&self) -> broadcast::Receiver<ReceiptWithHash> {
        self.receipt_sender.subscribe()
    }
}

impl Cache {
    pub fn new(receipt_buffer_size: usize) -> Self {
        Self {
            receipt_sender: broadcast::channel(receipt_buffer_size).0,
            ..Self::default()
        }
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

        self.set(CacheKey::PendingReceipts(block_number), &receipts, Some(10))?;

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

            // Reset highest index to 0 for new block
            if let Err(e) = self.set(CacheKey::HighestPayloadIndex, &0u64, Some(10)) {
                error!(
                    message = "failed to reset highest flash index",
                    error = %e
                );
            }
        } else {
            // Update highest index if current index is higher
            let current_highest = self.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap_or(0);
            if index > current_highest {
                if let Err(e) = self.set(CacheKey::HighestPayloadIndex, &index, Some(10)) {
                    error!(
                        message = "failed to update highest flash index",
                        error = %e
                    );
                }
            }
        }
    }

    pub fn process_payload(&self, payload: FlashblocksPayloadV1) {
        let msg_processing_start_time = Instant::now();

        // Convert metadata with error handling
        let metadata: Metadata = match serde_json::from_value(payload.metadata) {
            Ok(m) => m,
            Err(e) => {
                error!(
                    message = "failed to deserialize metadata",
                    error = %e
                );
                return;
            }
        };

        let block_number = metadata.block_number;
        let diff = payload.diff;
        let withdrawals = diff.withdrawals.clone();
        let diff_transactions = diff.transactions.clone();

        // Skip if index is 0 and base is not cached, likely the first payload
        // Can't do pending block with this because already missing blocks
        if payload.index != 0
            && self
                .get::<ExecutionPayloadBaseV1>(&CacheKey::Base(block_number))
                .is_none()
        {
            return;
        }

        // Track flashblock indices and record metrics
        self.update_flashblocks_index(payload.index);

        // Prevent updating to older blocks
        let current_block = self.get::<OpBlock>(&CacheKey::PendingBlock);
        if current_block.is_some() && current_block.unwrap().number > block_number {
            return;
        }

        // base only appears once in the first payload index
        let base = if let Some(base) = payload.base {
            if let Err(e) = self.set(CacheKey::Base(block_number), &base, Some(10)) {
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

        let transactions =
            match self.get_and_set_transactions(diff_transactions, payload.index, block_number) {
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
        if let Err(e) = self.set(CacheKey::PendingBlock, &block, Some(10)) {
            error!(
                message = "failed to set pending block in cache",
                error = %e
            );
            return;
        }

        // set block to block number as well
        if let Err(e) = self.set(CacheKey::Block(block_number), &block, Some(10)) {
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
        let _receipts =
            match self.get_and_set_all_receipts(payload.index, block_number, diff_receipts.clone())
            {
                Ok(receipts) => receipts,
                Err(e) => {
                    error!(
                        message = "failed to get and set all receipts",
                        error = %e
                    );
                    return;
                }
            };

        // Store account balances
        for (address, balance) in metadata.new_account_balances.iter() {
            if let Err(e) = self.set(
                CacheKey::AccountBalance(Address::from_str(address).unwrap()),
                &balance,
                Some(10),
            ) {
                error!(
                    message = "failed to set account balance in cache",
                    error = %e
                );
            }
        }

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
        if payload.index == 0 {
            info!(
                message = "block processing completed",
                processing_time = ?msg_processing_start_time.elapsed()
            );
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

        self.set(
            CacheKey::DiffTransactions(block_number),
            &transactions,
            Some(10),
        )?;

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
                if let Err(e) = self.set(
                    CacheKey::Transaction(transaction.tx_hash()),
                    &transaction,
                    Some(10),
                ) {
                    error!(
                        message = "failed to set transaction in cache",
                        error = %e
                    );
                    continue;
                }
                // update tx index
                if let Err(e) = self.set(
                    CacheKey::TransactionIndex(transaction.tx_hash()),
                    &idx,
                    Some(10),
                ) {
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
                        Some(10),
                    ) {
                        error!(
                            message = "failed to set transaction count in cache",
                            error = %e
                        );
                    }

                    // also keep track of sender of each transaction
                    if let Err(e) = self.set(
                        CacheKey::TransactionSender(transaction.tx_hash()),
                        &from,
                        Some(10),
                    ) {
                        error!(
                            message = "failed to set transaction sender in cache",
                            error = %e
                        );
                    }

                    // also keep track of the block number of each transaction
                    if let Err(e) = self.set(
                        CacheKey::TransactionBlockNumber(transaction.tx_hash()),
                        &block_number,
                        Some(10),
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
                if let Err(e) =
                    self.set(CacheKey::Receipt(transaction.tx_hash()), receipt, Some(10))
                {
                    error!(
                        message = "failed to set receipt in cache",
                        error = %e
                    );
                    continue;
                }
                // map receipt's block number as well
                if let Err(e) = self.set(
                    CacheKey::ReceiptBlock(transaction.tx_hash()),
                    &block_number,
                    Some(10),
                ) {
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
        ttl_secs: Option<u64>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let serialized = serde_json::to_vec(value)?;
        let entry = CacheEntry {
            value: serialized,
            expiry: ttl_secs.map(|secs| Instant::now() + Duration::from_secs(secs)),
        };

        let mut store = self.store.write().unwrap();
        store.insert(key, entry);
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, key: &CacheKey) -> Option<T> {
        let store = self.store.read().unwrap();
        store.get(key).and_then(|entry| {
            if entry.expiry.is_some_and(|e| Instant::now() > e) {
                return None;
            }
            serde_json::from_slice(&entry.value).ok()
        })
    }

    pub fn cleanup_expired(&self) {
        if let Ok(mut store) = self.store.write() {
            store.retain(|_, entry| {
                entry
                    .expiry
                    .map(|expiry| Instant::now() <= expiry)
                    .unwrap_or(true)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Receipt, TxReceipt};
    use alloy_primitives::{Address, B256, U256};
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpBlock;
    use rollup_boost::primitives::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::str::FromStr;

    fn create_first_payload() -> FlashblocksPayloadV1 {
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

        FlashblocksPayloadV1 {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(base),
            diff: delta,
            metadata: serde_json::to_value(metadata).unwrap(),
        }
    }

    // Create payload with specific index and block number
    fn create_payload_with_index(index: u64, block_number: u64) -> FlashblocksPayloadV1 {
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

        FlashblocksPayloadV1 {
            index,
            payload_id: PayloadId::new([0; 8]),
            base,
            diff: delta,
            metadata: serde_json::to_value(metadata).unwrap(),
        }
    }

    fn create_second_payload() -> FlashblocksPayloadV1 {
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
                    "0x1234567890123456789012345678901234567890".to_string(),
                    "0x1234".to_string(),
                );
                map
            },
        };

        FlashblocksPayloadV1 {
            index: 1,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta2,
            metadata: serde_json::to_value(metadata2).unwrap(),
        }
    }

    #[test]
    fn test_process_payload() {
        let cache = Arc::new(Cache::default());
        let mut receipt_receiver = cache.subscribe_to_receipts();

        let payload = create_first_payload();

        // Process first payload
        cache.process_payload(payload);

        let payload2 = create_second_payload();
        // Process second payload
        cache.process_payload(payload2);

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

        // Verify account balance was updated
        let balance = cache
            .get::<String>(&CacheKey::AccountBalance(
                Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            ))
            .unwrap();
        assert_eq!(balance, "0x1234");

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
        let cache = Arc::new(Cache::default());
        let metadata = Metadata {
            block_number: 1,
            receipts: HashMap::default(),
            new_account_balances: HashMap::default(),
        };

        let payload = FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0; 8]),
            index: 1, // Non-zero index but no base in cache
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: serde_json::to_value(metadata).unwrap(),
        };

        // Process payload
        cache.process_payload(payload);

        // Verify no block was stored, since it skips the first payload
        assert!(cache.get::<OpBlock>(&CacheKey::PendingBlock).is_none());
    }

    #[test]
    fn test_flash_block_tracking() {
        // Create cache
        let cache = Arc::new(Cache::default());
        // Process first block with 3 flash blocks
        // Block 1, payload 0 (starts a new block)
        let payload1_0 = create_payload_with_index(0, 1);
        cache.process_payload(payload1_0);

        // Check that highest_payload_index was set to 0
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);

        // Block 1, payload 1
        let payload1_1 = create_payload_with_index(1, 1);
        cache.process_payload(payload1_1);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 1);

        // Block 1, payload 2
        let payload1_2 = create_payload_with_index(2, 1);
        cache.process_payload(payload1_2);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 2);

        // Now start a new block (block 2, payload 0)
        let payload2_0 = create_payload_with_index(0, 2);
        cache.process_payload(payload2_0);

        // Check that highest_payload_index was reset to 0
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);

        // Block 2, payload 1 (out of order with payload 3)
        let payload2_1 = create_payload_with_index(1, 2);
        cache.process_payload(payload2_1);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 1);

        // Block 2, payload 3 (skipping 2)
        let payload2_3 = create_payload_with_index(3, 2);
        cache.process_payload(payload2_3);

        // Check that highest_payload_index was updated
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 3);

        // Block 2, payload 2 (out of order, should not change highest)
        let payload2_2 = create_payload_with_index(2, 2);
        cache.process_payload(payload2_2);

        // Check that highest_payload_index is still 3
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 3);

        // Start block 3, payload 0
        let payload3_0 = create_payload_with_index(0, 3);
        cache.process_payload(payload3_0);

        // Check that highest_payload_index was reset to 0
        // Also verify metric would have been recorded (though we can't directly check the metric's value)
        let highest = cache.get::<u64>(&CacheKey::HighestPayloadIndex).unwrap();
        assert_eq!(highest, 0);
    }
}
