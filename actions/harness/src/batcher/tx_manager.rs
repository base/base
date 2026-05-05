//! [`TxManager`] adapter that routes submissions through [`L1Miner`].

use std::sync::{Arc, Mutex};

use alloy_consensus::{
    SignableTransaction, TxEip1559, TxEip4844, TxEip4844Variant, TxEip4844WithSidecar, TxEnvelope,
};
use alloy_eips::{eip4844::Blob, eip7594::BlobTransactionSidecarVariant};
use alloy_primitives::{Address, B256, TxKind};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_batcher_source::L1HeadEvent;
use base_tx_manager::{
    BlobTxBuilder, SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError,
};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use crate::{L1Block, L1Miner};

/// A pending submission waiting for [`L1MinerTxManager::mine_block`] to fire its receipt.
pub struct Pending {
    /// Signed L1 transaction submitted to the miner.
    envelope: TxEnvelope,
    /// Blob sidecars for EIP-4844 submissions.
    blobs: Vec<(B256, Box<Blob>)>,
    /// Oneshot that resolves the driver's [`SendHandle`] with the mined block number.
    responder: oneshot::Sender<SendResponse>,
}

/// A signed L1 submission plus any blob sidecars it references.
#[derive(Debug, Clone)]
pub struct L1SignedSubmission {
    /// Signed transaction envelope submitted to L1.
    pub envelope: TxEnvelope,
    /// Blob sidecars keyed by the versioned hashes referenced by `envelope`.
    pub blobs: Vec<(B256, Box<Blob>)>,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("tx_hash", self.envelope.hash())
            .field("blobs", &self.blobs.len())
            .finish()
    }
}

/// Internal mutable state for [`L1MinerTxManager`]: pending and staged submissions.
#[derive(Debug, Default)]
pub struct Inner {
    pending: Vec<Pending>,
    staged: Vec<Pending>,
    /// Next nonce to use for signed production-mode transactions.
    next_nonce: u64,
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
    inbox_address: Address,
    signer: PrivateKeySigner,
    chain_id: u64,
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
    pub fn new(signer: PrivateKeySigner, inbox_address: Address, chain_id: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            inbox_address,
            signer,
            chain_id,
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

    /// Returns the number of submitted transactions waiting for inclusion receipts.
    pub fn staged_count(&self) -> usize {
        self.inner.lock().unwrap().staged.len()
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
            l1.submit_transaction(p.envelope.clone());
            for (hash, blob) in &p.blobs {
                l1.enqueue_blob(*hash, blob.clone());
            }
        }
        inner.staged.extend(to_stage);
        count
    }

    /// Fire receipt oneshots for staged items included in `block` and (if configured)
    /// publish an [`L1HeadEvent::NewHead`].
    ///
    /// Staged items without receipts in `block` remain staged. This models the
    /// production transaction manager's receipt polling: RPC submission can succeed
    /// before the transaction is included by L1.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    pub fn confirm_block(&self, block: &L1Block) {
        let responses = {
            let mut inner = self.inner.lock().unwrap();
            let staged = core::mem::take(&mut inner.staged);
            let mut still_staged = Vec::new();
            let mut responses = Vec::new();
            for p in staged {
                let tx_hash = *p.envelope.hash();
                if let Some(receipt) = block
                    .transaction_receipts
                    .iter()
                    .find(|receipt| receipt.transaction_hash == tx_hash)
                    .cloned()
                {
                    responses.push((p.responder, Ok(receipt)));
                } else {
                    still_staged.push(p);
                }
            }
            inner.staged = still_staged;
            responses
        };
        for (responder, response) in responses {
            let _ = responder.send(response);
        }
        if let Some(tx) = &self.l1_head_tx {
            let _ = tx.send(L1HeadEvent::NewHead(block.number()));
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
    /// queues. Items that have already been confirmed via [`confirm_block`]
    /// and are living in the driver's own `in_flight` set are not touched.
    /// Call this method *before* `confirm_staged` (or immediately after a
    /// yield has let the driver drain `in_flight`) to avoid leaving the
    /// driver in an inconsistent state.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`SendHandle`]: base_tx_manager::SendHandle
    /// [`confirm_block`]: L1MinerTxManager::confirm_block
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
        let block = l1.mine_block().clone();
        let block_number = block.number();
        self.confirm_block(&block);
        block_number
    }

    /// Build a signed transaction envelope and matching blob sidecar index for
    /// production-mode DA.
    pub fn sign_candidate(
        &self,
        candidate: &TxCandidate,
        nonce: u64,
    ) -> Result<L1SignedSubmission, TxManagerError> {
        let gas_limit = candidate.gas_limit.max(21_000);
        let to = candidate.to.unwrap_or(self.inbox_address);

        if candidate.blobs.is_empty() {
            let tx = TxEip1559 {
                chain_id: self.chain_id,
                nonce,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                gas_limit,
                to: TxKind::Call(to),
                value: candidate.value,
                input: candidate.tx_data.clone(),
                access_list: Default::default(),
            };
            let signature = self
                .signer
                .sign_hash_sync(&tx.signature_hash())
                .map_err(|e| TxManagerError::Sign(e.to_string()))?;
            return Ok(L1SignedSubmission {
                envelope: TxEnvelope::Eip1559(tx.into_signed(signature)),
                blobs: Vec::new(),
            });
        }

        let sidecar = BlobTxBuilder::build_sidecar(&candidate.blobs)?;
        let blob_hashes = sidecar.versioned_hashes().collect::<Vec<_>>();
        let blobs =
            blob_hashes.iter().copied().zip(candidate.blobs.iter().cloned()).collect::<Vec<_>>();
        let sidecar = BlobTransactionSidecarVariant::from(sidecar);
        let tx = TxEip4844 {
            chain_id: self.chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            to,
            value: candidate.value,
            access_list: Default::default(),
            blob_versioned_hashes: blob_hashes,
            max_fee_per_blob_gas: 1_000_000_000,
            input: candidate.tx_data.clone(),
        };
        let variant = TxEip4844Variant::TxEip4844WithSidecar(
            TxEip4844WithSidecar::from_tx_and_sidecar(tx, sidecar),
        );
        let signature = self
            .signer
            .sign_hash_sync(&variant.signature_hash())
            .map_err(|e| TxManagerError::Sign(e.to_string()))?;
        Ok(L1SignedSubmission {
            envelope: TxEnvelope::Eip4844(variant.into_signed(signature)),
            blobs,
        })
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

        let nonce = {
            let mut inner = self.inner.lock().unwrap();
            let nonce = inner.next_nonce;
            inner.next_nonce += 1;
            nonce
        };
        let signed = match self.sign_candidate(&candidate, nonce) {
            Ok(signed) => signed,
            Err(e) => {
                let (tx, rx) = oneshot::channel::<SendResponse>();
                let _ = tx.send(Err(e));
                return SendHandle::new(rx);
            }
        };

        let (responder, rx) = oneshot::channel::<SendResponse>();
        let pending = Pending { envelope: signed.envelope, blobs: signed.blobs, responder };
        self.inner.lock().unwrap().pending.push(pending);
        SendHandle::new(rx)
    }

    fn sender_address(&self) -> Address {
        self.signer.address()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_signer_local::PrivateKeySigner;
    use base_tx_manager::{TxCandidate, TxManager};

    use super::L1MinerTxManager;
    use crate::L1Miner;

    struct TxManagerFixture;

    impl TxManagerFixture {
        fn signer() -> PrivateKeySigner {
            PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).expect("valid test signer")
        }

        fn candidate(to: Address) -> TxCandidate {
            TxCandidate {
                tx_data: Bytes::from_static(b"\x00frame"),
                to: Some(to),
                gas_limit: 21_000,
                value: U256::ZERO,
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn confirm_block_keeps_unincluded_staged_submission_polling() {
        let inbox = Address::repeat_byte(0x42);
        let manager = L1MinerTxManager::new(TxManagerFixture::signer(), inbox, 1);
        let mut l1 = L1Miner::default();

        let handle = manager.send_async(TxManagerFixture::candidate(inbox)).await;
        assert_eq!(manager.pending_count(), 1);
        assert_eq!(manager.stage_n_to_l1(&mut l1, 1), 1);
        assert_eq!(manager.pending_count(), 0);
        assert_eq!(manager.staged_count(), 1);

        let genesis = l1.tip().clone();
        manager.confirm_block(&genesis);
        assert_eq!(manager.staged_count(), 1);

        let block = l1.mine_block().clone();
        manager.confirm_block(&block);
        assert_eq!(manager.staged_count(), 0);

        let receipt = handle.await.expect("staged transaction should confirm");
        assert_eq!(receipt.block_number, Some(block.number()));
        assert_eq!(receipt.transaction_index, Some(0));
        assert_eq!(receipt.to, Some(inbox));
    }
}
