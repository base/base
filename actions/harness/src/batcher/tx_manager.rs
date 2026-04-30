//! [`TxManager`] adapter that routes submissions through [`L1Miner`].

use std::sync::{Arc, Mutex};

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_eth::TransactionReceipt;
use base_batcher_source::L1HeadEvent;
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use crate::{L1Miner, PendingTx};

/// A pending submission waiting for [`L1MinerTxManager::mine_block`] to fire its receipt.
pub struct Pending {
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

/// Internal mutable state for [`L1MinerTxManager`]: pending and staged submissions.
#[derive(Debug, Default)]
pub struct Inner {
    pending: Vec<Pending>,
    staged: Vec<Pending>,
    /// Number of upcoming `send_async` calls to immediately fail with
    /// [`TxManagerError::Rpc`] before falling through to normal queuing.
    fail_remaining: usize,
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
/// When constructed with [`with_l1_head_tx`], [`mine_block`] automatically
/// sends an [`L1HeadEvent::NewHead`] to a paired [`ChannelL1HeadSource`] so
/// that the [`BatchDriver`] observes live L1 head updates.
///
/// [`send_async`]: L1MinerTxManager::send_async
/// [`mine_block`]: L1MinerTxManager::mine_block
/// [`with_l1_head_tx`]: L1MinerTxManager::with_l1_head_tx
/// [`BatchDriver`]: base_batcher_core::BatchDriver
/// [`ChannelL1HeadSource`]: base_batcher_source::ChannelL1HeadSource
#[derive(Debug, Clone)]
pub struct L1MinerTxManager {
    inner: Arc<Mutex<Inner>>,
    sender_address: Address,
    inbox_address: Address,
    /// Optional L1 head channel sender. When set, [`mine_block`] publishes
    /// `L1HeadEvent::NewHead(block_number)` so a paired [`ChannelL1HeadSource`]
    /// can advance the driver's L1 head.
    ///
    /// [`mine_block`]: L1MinerTxManager::mine_block
    /// [`ChannelL1HeadSource`]: base_batcher_source::ChannelL1HeadSource
    l1_head_tx: Option<mpsc::UnboundedSender<L1HeadEvent>>,
}

impl L1MinerTxManager {
    /// Create a new manager.
    pub fn new(sender_address: Address, inbox_address: Address) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            sender_address,
            inbox_address,
            l1_head_tx: None,
        }
    }

    /// Attach an L1 head channel sender.
    ///
    /// After each [`mine_block`] call, an [`L1HeadEvent::NewHead`] with the
    /// mined block number is sent to this channel. A [`BatchDriver`] constructed
    /// with the paired [`ChannelL1HeadSource`] will observe the update and advance
    /// its pipeline's L1 head accordingly.
    ///
    /// [`mine_block`]: L1MinerTxManager::mine_block
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`ChannelL1HeadSource`]: base_batcher_source::ChannelL1HeadSource
    pub fn with_l1_head_tx(mut self, tx: mpsc::UnboundedSender<L1HeadEvent>) -> Self {
        self.l1_head_tx = Some(tx);
        self
    }

    /// Returns the number of pending (not yet staged) submissions.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }

    /// Schedule the next `n` [`send_async`] calls to immediately resolve with
    /// [`TxManagerError::Rpc`], causing the [`BatchDriver`] to requeue the
    /// associated frames in the encoder pipeline.
    ///
    /// Failures are consumed one-per-call: setting `n = 3` means the next
    /// three separate `send_async` calls each fail, regardless of whether they
    /// carry the same or different frames.
    ///
    /// [`send_async`]: L1MinerTxManager::send_async
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    pub fn fail_next_n(&self, n: usize) {
        self.inner.lock().unwrap().fail_remaining += n;
    }

    /// Drop the first `n` pending submissions without staging them to L1.
    ///
    /// Returns the actual number dropped (≤ `n`). Use this to skip specific
    /// frame positions when testing non-sequential frame submission.
    pub fn drop_n(&self, n: usize) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let count = n.min(inner.pending.len());
        inner.pending.drain(..count);
        count
    }

    /// Move the first `n` pending submissions to the L1 miner's tx/blob queue
    /// and into the internal `staged` buffer. Does **not** mine a block.
    ///
    /// Returns the actual number of items staged (≤ `n`).
    pub fn stage_n_to_l1(&self, l1: &mut L1Miner, n: usize) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let count = n.min(inner.pending.len());
        let to_stage: Vec<Pending> = inner.pending.drain(..count).collect();
        for p in &to_stage {
            if let Some(tx) = &p.tx {
                l1.submit_tx(tx.clone());
            }
            for (hash, blob) in &p.blobs {
                l1.enqueue_blob(*hash, blob.clone());
            }
        }
        inner.staged.extend(to_stage);
        count
    }

    /// Fire receipt oneshots for all staged items and (if configured) publish
    /// an [`L1HeadEvent::NewHead`] for `block_number`.
    ///
    /// Call this after `l1.mine_block()` with the returned block number so that
    /// the [`BatchDriver`] receives correct receipts.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    pub fn confirm_all(&self, block_number: u64) {
        let staged = {
            let mut inner = self.inner.lock().unwrap();
            inner.staged.drain(..).collect::<Vec<_>>()
        };
        for p in staged {
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
        if let Some(tx) = &self.l1_head_tx {
            let _ = tx.send(L1HeadEvent::NewHead(block_number));
        }
    }

    /// Simulate an L1 reorg back to `block_number`.
    ///
    /// Calls [`L1Miner::reorg_to`] to truncate the canonical chain, fires a
    /// failure receipt for every pending and staged submission (since their
    /// inclusion block has been discarded or they are no longer valid), and
    /// publishes [`L1HeadEvent::NewHead`] so the [`BatchDriver`] observes
    /// the reorg.
    ///
    /// Both `pending` (not yet staged) and `staged` (submitted to L1 but not
    /// yet confirmed) items are drained. This ensures no [`SendHandle`] is
    /// left dangling, which would block the driver's `in_flight.next()`.
    ///
    /// # Ordering
    ///
    /// Failure receipts are fired *before* `L1HeadEvent::NewHead` is sent.
    /// This is intentional: the driver's `select!` loop prioritises receipt
    /// processing over head events, so firing receipts first ensures the
    /// driver requeues any failed frames before it advances its L1 head.
    ///
    /// # In-flight items
    ///
    /// This method only covers items still in the `pending` or `staged`
    /// queues. Items that have already been confirmed via [`confirm_all`]
    /// and are living in the driver's own `in_flight` set are not touched.
    /// Call this method *before* `confirm_staged` (or immediately after a
    /// yield has let the driver drain `in_flight`) to avoid leaving the
    /// driver in an inconsistent state.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`SendHandle`]: base_tx_manager::SendHandle
    /// [`confirm_all`]: L1MinerTxManager::confirm_all
    pub fn reorg_to(&self, block_number: u64, l1: &mut L1Miner) {
        l1.reorg_to(block_number).expect("reorg_to should not fail");
        let (pending, staged) = {
            let mut inner = self.inner.lock().unwrap();
            let pending: Vec<Pending> = inner.pending.drain(..).collect();
            let staged: Vec<Pending> = inner.staged.drain(..).collect();
            (pending, staged)
        };
        let drained = pending.len() + staged.len();
        for p in pending.into_iter().chain(staged) {
            let _ = p.responder.send(Err(TxManagerError::Rpc("reorg".to_string())));
        }
        if let Some(tx) = &self.l1_head_tx {
            let _ = tx.send(L1HeadEvent::NewHead(block_number));
        }
        info!(block_number = %block_number, drained = %drained, "simulated L1 reorg");
    }

    /// Submit all pending transactions/blobs to `l1`, mine one block, resolve
    /// all waiting [`SendHandle`]s with the real mined block number, and
    /// (if configured) publish the block number to the L1 head channel.
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
        self.stage_n_to_l1(l1, usize::MAX);
        let block_number = l1.mine_block().number();
        self.confirm_all(block_number);
        block_number
    }
}

impl TxManager for L1MinerTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.send_async(candidate).await.await
    }

    async fn send_async(&self, candidate: TxCandidate) -> SendHandle {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.fail_remaining > 0 {
                inner.fail_remaining -= 1;
                let (tx, rx) = oneshot::channel::<SendResponse>();
                let _ =
                    tx.send(Err(TxManagerError::Rpc("simulated submission failure".to_string())));
                return SendHandle::new(rx);
            }
        }

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
                blobs: candidate.blobs.iter().map(|b| (B256::ZERO, b.clone())).collect(),
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
