//! [`TxManager`] adapter that routes submissions through [`L1Miner`].

use std::sync::{Arc, Mutex};

use alloy_consensus::{
    SignableTransaction, TxEip1559, TxEip4844, TxEip4844Variant, TxEip4844WithSidecar, TxEnvelope,
};
use alloy_eips::{eip4844::Blob, eip7594::BlobTransactionSidecarVariant};
use alloy_primitives::{Address, B256, TxKind};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{
    BlobTxBuilder, SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError,
};
use tokio::sync::oneshot;

use crate::{L1Block, L1Miner};

/// A submission waiting to be staged, then for [`L1MinerTxManager::confirm_block`] to fire
/// its receipt.
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
    /// Signed submissions not yet handed to the miner.
    pending: Vec<Pending>,
    /// Submissions in the miner's queue, waiting for a receipt.
    staged: Vec<Pending>,
    /// Nonce of the next signed transaction.
    next_nonce: u64,
    /// Number of upcoming `send_async` calls to immediately fail with
    /// [`TxManagerError::Rpc`] before falling through to normal queuing.
    fail_remaining: usize,
}

/// Adapts [`L1Miner`] to the [`TxManager`] trait for action tests.
///
/// [`send_async`] signs a [`TxCandidate`], queues it as pending and returns a [`SendHandle`]
/// that resolves once the transaction is staged to the miner and [`confirm_block`] sees it
/// mined. The spawned [`BatchDriver`] task suspends on these handles; [`Batcher`] stages,
/// mines and confirms from the test side.
///
/// [`L1MinerTxManager`] is cheaply cloneable (Arc bump). Pass one clone to
/// [`BatchDriver`] and retain the other for the test.
///
/// [`send_async`]: L1MinerTxManager::send_async
/// [`confirm_block`]: L1MinerTxManager::confirm_block
/// [`BatchDriver`]: base_batcher_core::BatchDriver
/// [`Batcher`]: crate::Batcher
#[derive(Debug, Clone)]
pub struct L1MinerTxManager {
    /// Pending and staged submissions, shared with the driver's clone.
    inner: Arc<Mutex<Inner>>,
    /// Signs every submission. Its address is the batcher's sender.
    signer: PrivateKeySigner,
    /// L1 chain id stamped on every signed transaction.
    chain_id: u64,
}

impl L1MinerTxManager {
    /// Create a new manager.
    pub fn new(signer: PrivateKeySigner, chain_id: u64) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner::default())), signer, chain_id }
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

    /// Fire receipt oneshots for staged items included in `block`.
    ///
    /// Staged items without receipts in `block` remain staged. This models the
    /// production transaction manager's receipt polling: RPC submission can succeed
    /// before the transaction is included by L1.
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
    }

    /// Build the signed transaction envelope of `candidate` and the blob sidecars it
    /// references.
    pub fn sign_candidate(
        &self,
        candidate: &TxCandidate,
        nonce: u64,
    ) -> Result<L1SignedSubmission, TxManagerError> {
        let gas_limit = candidate.gas_limit.max(21_000);
        let to = candidate.to.expect("the driver addresses every candidate to the inbox");

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
