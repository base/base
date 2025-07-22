use alloy_primitives::{Address, B256};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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
    // Overrides for eth_call based on flashblocks executed txs
    PendingOverrides, // pending
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
            CacheKey::PendingOverrides => write!(f, "pending_storage_slots"),
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
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Cache {
    pub fn set<T: Serialize>(
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
