//! `OutputProposer` trait and `ProposalSubmitter` implementation for L1 transaction submission.
//!
//! Submits output proposals by creating new dispute games via `DisputeGameFactory.createWithInitData()`.
//! Delegates all transaction lifecycle management (nonce, fees, signing, resubmission)
//! to the shared [`TxManager`].

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use base_proof_contracts::{encode_create_calldata, encode_extra_data};
use base_proof_primitives::{ProofEncoder, Proposal};
use base_proof_submission::{AggregateProofSubmitter, ProofSubmissionError};
use base_tx_manager::{TxCandidate, TxManager};
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
    tx_manager: T,
    factory_address: Address,
    game_type: u32,
    init_bond: U256,
}

impl<T> ProposalSubmitter<T> {
    /// Creates a new [`ProposalSubmitter`] backed by the given transaction manager.
    pub const fn new(
        tx_manager: T,
        factory_address: Address,
        game_type: u32,
        init_bond: U256,
    ) -> Self {
        Self { tx_manager, factory_address, game_type, init_bond }
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

        let receipt = self.tx_manager.send(candidate).await.map_err(ProofSubmissionError::from)?;

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

        let receipt = AggregateProofSubmitter::new(&self.tx_manager)
            .verify_proposal_proof(game_address, proof_bytes)
            .await?;

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
    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_tx_manager::{SendHandle, SendResponse, TxManagerError};

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

    fn test_submitter(response: SendResponse) -> ProposalSubmitter<MockTxManager> {
        ProposalSubmitter::new(
            MockTxManager { response },
            Address::repeat_byte(0x01),
            1,
            U256::from(100_u64),
        )
    }

    #[derive(Debug)]
    struct MockTxManager {
        response: SendResponse,
    }

    impl TxManager for MockTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            self.response.clone()
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("not needed for these tests")
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
}
