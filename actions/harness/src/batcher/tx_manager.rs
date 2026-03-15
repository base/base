//! [`TxManager`] adapter that routes submissions through [`L1Miner`].

use std::sync::{Arc, Mutex};

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_eth::TransactionReceipt;
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
use tokio::sync::oneshot;

use crate::{L1Miner, PendingTx};

/// A pending submission waiting for [`L1MinerTxManager::mine_block`] to fire its receipt.
struct Pending {
    /// Calldata transaction, or `None` for blob-only submissions.
    tx: Option<PendingTx>,
    /// Blob sidecars for EIP-4844 submissions.
    blobs: Vec<(B256, Box<Blob>)>,
    /// Oneshot that resolves the driver's [`SendHandle`] with the mined block number.
    responder: oneshot::Sender<SendResponse>,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("has_tx", &self.tx.is_some())
            .field("blobs", &self.blobs.len())
            .finish()
    }
}

#[derive(Debug, Default)]
struct Inner {
    pending: Vec<Pending>,
}

/// Adapts [`L1Miner`] to the [`TxManager`] trait for action tests.
///
/// [`send_async`] enqueues a [`TxCandidate`] and returns a [`SendHandle`] that
/// resolves when [`mine_block`] is called. The spawned [`BatchDriver`] task
/// suspends on these handles; calling [`mine_block`] after
/// `tokio::task::yield_now().await` gives the driver time to populate its
/// in-flight set before receipts are fired.
///
/// [`L1MinerTxManager`] is cheaply cloneable (Arc bump). Pass one clone to
/// [`BatchDriver`] and retain the other for [`mine_block`] calls from the test.
///
/// [`send_async`]: L1MinerTxManager::send_async
/// [`mine_block`]: L1MinerTxManager::mine_block
/// [`BatchDriver`]: base_batcher_core::BatchDriver
#[derive(Debug, Clone)]
pub struct L1MinerTxManager {
    inner: Arc<Mutex<Inner>>,
    sender_address: Address,
    inbox_address: Address,
}

impl L1MinerTxManager {
    /// Create a new manager.
    pub fn new(sender_address: Address, inbox_address: Address) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner::default())), sender_address, inbox_address }
    }

    /// Submit all pending transactions/blobs to `l1`, mine one block, and resolve
    /// all waiting [`SendHandle`]s with the real mined block number.
    ///
    /// # Timing
    ///
    /// Call this after `tokio::task::yield_now().await` so the spawned
    /// [`BatchDriver`] task has had one scheduling turn to process blocks, call
    /// [`send_async`] for each submission, and suspend waiting on the oneshot
    /// receivers.
    ///
    /// On a `current_thread` tokio runtime (the default for `#[tokio::test]`) a
    /// single yield is sufficient: [`InMemoryBlockSource::next`] and
    /// [`send_async`] both complete without suspending, so the driver runs the
    /// full encoding and submission loop in one turn before sticking on
    /// `in_flight.next().await`.
    ///
    /// [`send_async`]: L1MinerTxManager::send_async
    /// [`InMemoryBlockSource::next`]: base_batcher_source::test_utils::InMemoryBlockSource
    pub fn mine_block(&self, l1: &mut L1Miner) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        for p in &inner.pending {
            if let Some(tx) = &p.tx {
                l1.submit_tx(tx.clone());
            }
            for (hash, blob) in &p.blobs {
                l1.enqueue_blob(*hash, blob.clone());
            }
        }
        let block_number = l1.mine_block().number();
        for p in inner.pending.drain(..) {
            let receipt = TransactionReceipt {
                inner: ReceiptEnvelope::Legacy(ReceiptWithBloom {
                    receipt: Receipt {
                        status: Eip658Value::Eip658(true),
                        cumulative_gas_used: 21_000,
                        logs: vec![],
                    },
                    logs_bloom: Bloom::ZERO,
                }),
                transaction_hash: B256::ZERO,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(block_number),
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: Some(self.inbox_address),
                contract_address: None,
            };
            let _ = p.responder.send(Ok(receipt));
        }
        block_number
    }
}

impl TxManager for L1MinerTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.send_async(candidate).await.await
    }

    async fn send_async(&self, candidate: TxCandidate) -> SendHandle {
        let (responder, rx) = oneshot::channel::<SendResponse>();
        let pending = if candidate.blobs.is_empty() {
            Pending {
                tx: Some(PendingTx {
                    from: self.sender_address,
                    to: self.inbox_address,
                    input: candidate.tx_data,
                }),
                blobs: Vec::new(),
                responder,
            }
        } else {
            Pending {
                tx: None,
                blobs: candidate.blobs.iter().map(|b| (B256::ZERO, Box::new(*b))).collect(),
                responder,
            }
        };
        self.inner.lock().unwrap().pending.push(pending);
        SendHandle::new(rx)
    }

    fn sender_address(&self) -> Address {
        self.sender_address
    }
}
