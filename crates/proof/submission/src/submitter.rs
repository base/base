//! Aggregate verifier proof transaction submission.

use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::TransactionReceipt;
use base_proof_contracts::encode_verify_proposal_proof_calldata;
use base_tx_manager::{TxCandidate, TxManager};

use crate::ProofSubmissionError;

/// Submits proof bytes to an existing aggregate verifier dispute game.
#[derive(Debug)]
pub struct AggregateProofSubmitter<'a, T> {
    tx_manager: &'a T,
}

impl<'a, T: TxManager> AggregateProofSubmitter<'a, T> {
    /// Creates a submitter backed by the given transaction manager.
    pub const fn new(tx_manager: &'a T) -> Self {
        Self { tx_manager }
    }

    /// Submits `AggregateVerifier.verifyProposalProof(bytes)` to an existing game.
    /// Callers provide already-encoded TEE or ZK proof bytes.
    pub async fn verify_proposal_proof(
        &self,
        game_address: Address,
        proof_bytes: Bytes,
    ) -> Result<TransactionReceipt, ProofSubmissionError> {
        let calldata = encode_verify_proposal_proof_calldata(proof_bytes);
        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(game_address),
            value: U256::ZERO,
            ..Default::default()
        };

        let receipt = self.tx_manager.send(candidate).await.map_err(ProofSubmissionError::from)?;
        let tx_hash = receipt.transaction_hash;

        if !receipt.inner.status() {
            return Err(ProofSubmissionError::TxReverted(tx_hash));
        }

        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};

    use super::AggregateProofSubmitter;
    use crate::ProofSubmissionError;

    fn receipt_with_status(success: bool, tx_hash: B256) -> TransactionReceipt {
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
            transaction_hash: tx_hash,
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

    #[derive(Debug)]
    struct MockTxManager {
        response: Mutex<Option<SendResponse>>,
        candidate: Mutex<Option<TxCandidate>>,
    }

    impl MockTxManager {
        fn new(response: SendResponse) -> Self {
            Self { response: Mutex::new(Some(response)), candidate: Mutex::new(None) }
        }

        fn take_candidate(&self) -> Option<TxCandidate> {
            self.candidate.lock().unwrap().take()
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> SendResponse {
            *self.candidate.lock().unwrap() = Some(candidate);
            self.response.lock().unwrap().take().expect("MockTxManager response already consumed")
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("not needed for these tests")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[tokio::test]
    async fn verify_proposal_proof_sends_encoded_calldata_to_game() {
        let game_address = Address::repeat_byte(0x11);
        let proof_bytes = Bytes::from_static(&[0x00, 0xab, 0xcd]);
        let tx_hash = B256::repeat_byte(0xaa);
        let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));
        let submitter = AggregateProofSubmitter::new(&tx_manager);

        let receipt = submitter.verify_proposal_proof(game_address, proof_bytes.clone()).await;

        assert_eq!(receipt.unwrap().transaction_hash, tx_hash);
        let candidate = tx_manager.take_candidate().unwrap();
        assert_eq!(candidate.to, Some(game_address));
        assert_eq!(candidate.value, U256::ZERO);
        assert_eq!(
            candidate.tx_data,
            base_proof_contracts::encode_verify_proposal_proof_calldata(proof_bytes)
        );
    }

    #[tokio::test]
    async fn verify_proposal_proof_maps_reverted_receipt() {
        let tx_hash = B256::repeat_byte(0xbb);
        let tx_manager = MockTxManager::new(Ok(receipt_with_status(false, tx_hash)));
        let submitter = AggregateProofSubmitter::new(&tx_manager);

        let err = submitter
            .verify_proposal_proof(Address::ZERO, Bytes::from_static(&[0x00]))
            .await
            .unwrap_err();

        assert!(matches!(err, ProofSubmissionError::TxReverted(hash) if hash == tx_hash));
    }

    #[tokio::test]
    async fn verify_proposal_proof_classifies_tx_manager_errors() {
        let tx_manager = MockTxManager::new(Err(TxManagerError::ExecutionReverted {
            reason: Some("AlreadyProven(0)".to_string()),
            data: None,
        }));
        let submitter = AggregateProofSubmitter::new(&tx_manager);

        let err = submitter
            .verify_proposal_proof(Address::ZERO, Bytes::from_static(&[0x00]))
            .await
            .unwrap_err();

        assert!(matches!(err, ProofSubmissionError::ProofAlreadyVerified));
    }
}
