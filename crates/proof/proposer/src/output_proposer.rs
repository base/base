//! `OutputProposer` trait and `ProposalSubmitter` implementation for L1 transaction submission.
//!
//! Submits output proposals by creating new dispute games via `DisputeGameFactory.createWithInitData()`.
//! Delegates all transaction lifecycle management (nonce, fees, signing, resubmission)
//! to the shared [`TxManager`].

use alloy_primitives::{Address, B256, U256, keccak256};
use async_trait::async_trait;
use base_proof_contracts::{encode_create_calldata, encode_extra_data};
use base_proof_primitives::{ProofEncoder, Proposal};
use base_proof_submission::ProofSubmissionError;
use base_tx_manager::{SubmissionHandle, SubmissionResult, TxCandidate, TxManager};
use tokio::sync::Mutex;
use tracing::info;

use crate::error::ProposerError;

/// Trait for submitting output proposals to L1 via dispute game creation.
#[async_trait]
pub trait OutputProposer: Send + Sync {
    /// Creates a new dispute game for the given proposal.
    async fn propose_output(
        &self,
        proposal: &Proposal,
        parent_address: Address,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError>;

    /// Attaches a proof to an already-existing matching dispute game.
    async fn verify_proposal_proof(
        &self,
        game_address: Address,
        proposal: &Proposal,
    ) -> Result<(), ProposerError>;
}

/// No-op output proposer that logs proposals without submitting transactions.
#[derive(Debug)]
pub struct DryRunProposer;

#[async_trait]
impl OutputProposer for DryRunProposer {
    async fn propose_output(
        &self,
        proposal: &Proposal,
        parent_address: Address,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        info!(
            l2_block_number = proposal.l2_block_number,
            parent_address = %parent_address,
            output_root = ?proposal.output_root,
            intermediate_roots_count = intermediate_roots.len(),
            "DRY RUN: would create dispute game (skipping submission)"
        );
        Ok(())
    }

    async fn verify_proposal_proof(
        &self,
        game_address: Address,
        proposal: &Proposal,
    ) -> Result<(), ProposerError> {
        info!(
            game_address = %game_address,
            l2_block_number = proposal.l2_block_number,
            output_root = ?proposal.output_root,
            "DRY RUN: would attach proof to existing dispute game (skipping submission)"
        );
        Ok(())
    }
}

/// Submits output proposals to L1 via the [`TxManager`].
#[derive(Debug)]
pub struct ProposalSubmitter<T> {
    /// Transaction lifecycle service used for L1 publication.
    tx_manager: T,
    /// Dispute-game factory receiving proposal transactions.
    factory_address: Address,
    /// On-chain game type encoded into proposal calldata.
    game_type: u32,
    /// ETH bond attached to each new game transaction.
    init_bond: U256,
    /// Detached submission reconciled before another proposal may be sent.
    pending: Mutex<Option<PendingSubmission>>,
}

/// Status retained when a proposal caller detaches before nonce resolution.
#[derive(Debug, Clone)]
struct PendingSubmission {
    /// Stable identity of every field shaping the submitted transaction.
    fingerprint: B256,
    /// Persistent handle used to recover the eventual terminal outcome.
    handle: SubmissionHandle,
}

impl<T> ProposalSubmitter<T> {
    /// Creates a new [`ProposalSubmitter`] backed by the given transaction manager.
    pub fn new(tx_manager: T, factory_address: Address, game_type: u32, init_bond: U256) -> Self {
        Self { tx_manager, factory_address, game_type, init_bond, pending: Mutex::new(None) }
    }
}

impl<T: TxManager> ProposalSubmitter<T> {
    /// Submits one candidate without duplicating a detached matching operation.
    ///
    /// The retained handle survives the proposer's outer timeout. A later
    /// pipeline retry first reconciles that submission before it may allocate
    /// another nonce.
    async fn submit_candidate(&self, candidate: TxCandidate) -> SubmissionResult {
        let fingerprint = Self::candidate_fingerprint(&candidate);
        let (handle, submission_id) = {
            // Phase 1: serialize lifecycle hand-off. The guard may wait for an
            // older detached nonce, but no RPC or signing work runs under it.
            let mut pending = self.pending.lock().await;
            if let Some(existing) = pending.clone() {
                let outcome = existing.handle.wait().await;
                *pending = None;

                // An outer timeout retries the same proposal. Reuse the prior
                // terminal outcome rather than submitting identical calldata
                // at the next nonce.
                if existing.fingerprint == fingerprint {
                    return outcome;
                }
            }

            // Phase 2: retain a clone before awaiting. If this future is
            // dropped, the next invocation can still reconcile the submission.
            let handle = self.tx_manager.submit(candidate);
            let submission_id = handle.id();
            *pending = Some(PendingSubmission { fingerprint, handle: handle.clone() });
            (handle, submission_id)
        };

        // Phase 3: wait until the manager resolves nonce ownership.
        let outcome = handle.wait().await;
        // Clear only our own generation; another invocation may already have
        // installed a later submission after reconciling this one.
        let mut pending = self.pending.lock().await;
        if pending.as_ref().is_some_and(|current| current.handle.id() == submission_id) {
            *pending = None;
        }
        outcome
    }

    /// Computes a stable identity for every transaction-shaping candidate field.
    fn candidate_fingerprint(candidate: &TxCandidate) -> B256 {
        let mut encoded =
            Vec::with_capacity(1 + Address::len_bytes() + 8 + 32 + candidate.tx_data.len());
        match candidate.to {
            Some(to) => {
                encoded.push(1);
                encoded.extend_from_slice(to.as_slice());
            }
            None => encoded.push(0),
        }
        encoded.extend_from_slice(&candidate.gas_limit.to_be_bytes());
        encoded.extend_from_slice(&candidate.value.to_be_bytes::<32>());
        encoded.extend_from_slice(&candidate.tx_data);
        for blob in candidate.blobs.iter() {
            encoded.extend_from_slice(keccak256(blob.as_slice()).as_slice());
        }
        keccak256(encoded)
    }
}

#[async_trait]
impl<T: TxManager + 'static> OutputProposer for ProposalSubmitter<T> {
    async fn propose_output(
        &self,
        proposal: &Proposal,
        parent_address: Address,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        let l2_block_number = proposal.l2_block_number;
        let proof_data =
            proposal.build_proof_data().map_err(|e| ProposerError::Internal(e.to_string()))?;
        let extra_data = encode_extra_data(l2_block_number, parent_address, intermediate_roots);
        let calldata =
            encode_create_calldata(self.game_type, proposal.output_root, extra_data, proof_data);
        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.factory_address),
            value: self.init_bond,
            ..Default::default()
        };

        info!(
            l2_block_number,
            factory = %self.factory_address,
            game_type = self.game_type,
            parent_address = %parent_address,
            tx_data_len = candidate.tx_data.len(),
            "Creating dispute game"
        );

        let receipt = self.submit_candidate(candidate).await.map_err(ProofSubmissionError::from)?;

        if !receipt.inner.status() {
            return Err(ProofSubmissionError::TxReverted(receipt.transaction_hash).into());
        }

        info!(
            tx_hash = %receipt.transaction_hash,
            l2_block_number,
            block_number = receipt.block_number,
            "Proposal transaction confirmed"
        );
        Ok(())
    }

    async fn verify_proposal_proof(
        &self,
        game_address: Address,
        proposal: &Proposal,
    ) -> Result<(), ProposerError> {
        let l2_block_number = proposal.l2_block_number;
        let proof_bytes = ProofEncoder::encode_dispute_proof_bytes(&proposal.signature)
            .map_err(|e| ProposerError::Internal(e.to_string()))?;

        info!(
            l2_block_number,
            game_address = %game_address,
            proof_bytes_len = proof_bytes.len(),
            "Attaching proof to existing dispute game"
        );

        let candidate = TxCandidate {
            tx_data: base_proof_contracts::encode_verify_proposal_proof_calldata(proof_bytes),
            to: Some(game_address),
            value: U256::ZERO,
            ..Default::default()
        };
        let receipt = self.submit_candidate(candidate).await.map_err(ProofSubmissionError::from)?;
        if !receipt.inner.status() {
            return Err(ProofSubmissionError::TxReverted(receipt.transaction_hash).into());
        }

        info!(
            tx_hash = %receipt.transaction_hash,
            l2_block_number,
            game_address = %game_address,
            block_number = receipt.block_number,
            "Proposal proof attachment transaction confirmed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_tx_manager::{
        SubmissionHandle, SubmissionId, SubmissionResult, SubmissionSnapshot, SubmissionStatus,
        TxManagerError,
    };
    use tokio::sync::watch;

    use super::*;
    use crate::test_utils::test_proposal;

    fn receipt_with_status(success: bool) -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(success),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    fn test_submitter(response: SubmissionResult) -> ProposalSubmitter<MockTxManager> {
        ProposalSubmitter::new(
            MockTxManager { response },
            Address::repeat_byte(0x01),
            1,
            U256::from(100_u64),
        )
    }

    #[derive(Debug)]
    struct MockTxManager {
        response: SubmissionResult,
    }

    impl TxManager for MockTxManager {
        fn submit(&self, _candidate: TxCandidate) -> SubmissionHandle {
            SubmissionHandle::resolved(self.response.clone())
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[tokio::test]
    async fn propose_output_reverted() {
        let submitter = test_submitter(Ok(receipt_with_status(false)));
        let err =
            submitter.propose_output(&test_proposal(200), Address::ZERO, &[]).await.unwrap_err();
        assert!(matches!(err, ProposerError::Submission(ProofSubmissionError::TxReverted(_))));
    }

    #[tokio::test]
    async fn propose_output_tx_manager_error() {
        let submitter = test_submitter(Err(TxManagerError::NonceTooLow));
        let err =
            submitter.propose_output(&test_proposal(200), Address::ZERO, &[]).await.unwrap_err();
        assert!(
            matches!(
                err,
                ProposerError::Submission(ProofSubmissionError::TxManager(
                    TxManagerError::NonceTooLow
                ))
            ),
            "expected TxManager(NonceTooLow), got {err:?}",
        );
    }

    #[derive(Debug, Clone, Default)]
    struct TrackingTxManager {
        inner: Arc<TrackingTxManagerInner>,
    }

    #[derive(Debug, Default)]
    struct TrackingTxManagerInner {
        sends: AtomicUsize,
        pending: StdMutex<Option<watch::Sender<SubmissionSnapshot>>>,
    }

    impl TrackingTxManager {
        fn send_count(&self) -> usize {
            self.inner.sends.load(Ordering::SeqCst)
        }

        fn resolve(&self, outcome: SubmissionResult) {
            let status_tx = self.inner.pending.lock().unwrap().take().unwrap();
            status_tx.send_replace(SubmissionSnapshot {
                id: SubmissionId::new(1),
                status: SubmissionStatus::Resolved(Box::new(outcome)),
            });
        }
    }

    impl TxManager for TrackingTxManager {
        fn submit(&self, _candidate: TxCandidate) -> SubmissionHandle {
            self.inner.sends.fetch_add(1, Ordering::SeqCst);
            let (status_tx, status_rx) =
                watch::channel(SubmissionSnapshot::staged(SubmissionId::new(1)));
            *self.inner.pending.lock().unwrap() = Some(status_tx);
            SubmissionHandle::new(status_rx)
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[tokio::test]
    async fn retry_waits_for_detached_matching_submission() {
        let manager = TrackingTxManager::default();
        let submitter =
            ProposalSubmitter::new(manager.clone(), Address::repeat_byte(1), 1, U256::from(100));
        let proposal = test_proposal(200);

        let first = tokio::time::timeout(
            Duration::from_millis(1),
            submitter.propose_output(&proposal, Address::ZERO, &[]),
        )
        .await;
        assert!(first.is_err());
        assert_eq!(manager.send_count(), 1);

        let retry = submitter.propose_output(&proposal, Address::ZERO, &[]);
        tokio::pin!(retry);
        tokio::select! {
            result = &mut retry => panic!("retry resolved before detached submission: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
        assert_eq!(manager.send_count(), 1);

        manager.resolve(Ok(receipt_with_status(true)));
        retry.await.unwrap();
        assert_eq!(manager.send_count(), 1);
    }
}
