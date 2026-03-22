//! `OutputProposer` trait and `ProposalSubmitter` implementation for L1 transaction submission.
//!
//! Submits output proposals by creating new dispute games via `DisputeGameFactory.create()`.
//! Delegates all transaction lifecycle management (nonce, fees, signing, resubmission)
//! to the shared [`TxManager`].

use std::sync::LazyLock;

use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use base_enclave::ProofEncoder;
use base_proof_contracts::{
    encode_create_calldata, encode_extra_data, game_already_exists_selector,
};
use base_proof_primitives::Proposal;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tracing::info;

use crate::error::ProposerError;

/// Hex-encoded `GameAlreadyExists` selector, computed once.
static GAME_ALREADY_EXISTS_HEX: LazyLock<String> =
    LazyLock::new(|| alloy_primitives::hex::encode(game_already_exists_selector()));

/// Classifies a [`TxManagerError`] into a [`ProposerError`].
///
/// Checks the structured revert reason and raw data for the
/// `GameAlreadyExists` selector first, then falls back to searching the
/// Display string for non-`ExecutionReverted` variants (e.g. `Rpc`).
/// Returns [`ProposerError::GameAlreadyExists`] if found, otherwise
/// wrapping it as [`ProposerError::TxManager`].
fn classify_tx_manager_error(err: TxManagerError) -> ProposerError {
    if let TxManagerError::ExecutionReverted { ref reason, ref data } = err {
        // Check reason string for GameAlreadyExists.
        if reason.as_deref().is_some_and(|r| r.contains("GameAlreadyExists")) {
            return ProposerError::GameAlreadyExists;
        }
        // Check raw data for the GameAlreadyExists selector.
        if data.as_ref().is_some_and(|d| d.len() >= 4 && d[..4] == game_already_exists_selector()) {
            return ProposerError::GameAlreadyExists;
        }
        // Structured fields exhausted — no need to format the Display
        // string since it won't contain additional GameAlreadyExists info.
        return ProposerError::TxManager(err);
    }
    // Fallback: check Display output for non-ExecutionReverted variants
    // (e.g. Rpc) that may carry "GameAlreadyExists" in their message.
    let msg = err.to_string();
    if msg.contains(GAME_ALREADY_EXISTS_HEX.as_str()) || msg.contains("GameAlreadyExists") {
        return ProposerError::GameAlreadyExists;
    }
    ProposerError::TxManager(err)
}

/// Builds the proof data for `AggregateVerifier.initialize()`.
///
/// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)` = 130 bytes.
///
/// Matches Go's `buildProofData()` in `driver.go`.
pub fn build_proof_data(proposal: &Proposal) -> Result<Bytes, ProposerError> {
    ProofEncoder::encode_proof_bytes(
        &proposal.signature,
        proposal.l1_origin_hash,
        proposal.l1_origin_number,
    )
    .map_err(|e| ProposerError::Internal(e.to_string()))
}

/// Returns true if the error indicates the game already exists.
pub const fn is_game_already_exists(e: &ProposerError) -> bool {
    matches!(e, ProposerError::GameAlreadyExists)
}

/// Trait for submitting output proposals to L1 via dispute game creation.
#[async_trait]
pub trait OutputProposer: Send + Sync {
    /// Creates a new dispute game for the given proposal.
    async fn propose_output(
        &self,
        proposal: &Proposal,
        l2_block_number: u64,
        parent_index: u32,
        intermediate_roots: &[B256],
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
        l2_block_number: u64,
        parent_index: u32,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        info!(
            l2_block_number,
            parent_index,
            output_root = ?proposal.output_root,
            intermediate_roots_count = intermediate_roots.len(),
            "DRY RUN: would create dispute game (skipping submission)"
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

impl<T: TxManager> ProposalSubmitter<T> {
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
        l2_block_number: u64,
        parent_index: u32,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        let proof_data = build_proof_data(proposal)?;
        let extra_data = encode_extra_data(l2_block_number, parent_index, intermediate_roots);
        let calldata =
            encode_create_calldata(self.game_type, proposal.output_root, extra_data, proof_data);

        info!(
            l2_block_number,
            factory = %self.factory_address,
            game_type = self.game_type,
            calldata = %calldata,
            parent_index,
            "Creating dispute game"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.factory_address),
            value: self.init_bond,
            ..Default::default()
        };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        let receipt = self.tx_manager.send(candidate).await.map_err(classify_tx_manager_error)?;

        let tx_hash = receipt.transaction_hash;

        if !receipt.inner.status() {
            return Err(ProposerError::TxReverted(format!("transaction {tx_hash} reverted")));
        }

        info!(
            %tx_hash,
            l2_block_number,
            block_number = receipt.block_number,
            "Proposal transaction confirmed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_enclave::PROOF_TYPE_TEE;
    use base_tx_manager::{SendHandle, SendResponse, TxManagerError};

    use super::*;

    fn test_proposal() -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(0x01),
            signature: {
                let mut sig = vec![0xab; 65];
                sig[64] = 1;
                Bytes::from(sig)
            },
            l1_origin_hash: B256::repeat_byte(0x02),
            l1_origin_number: U256::from(300),
            l2_block_number: U256::from(200),
            prev_output_root: B256::repeat_byte(0x03),
            config_hash: B256::repeat_byte(0x04),
        }
    }

    /// Builds a minimal [`TransactionReceipt`] with the given status and hash.
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

    /// Mock transaction manager for testing.
    #[derive(Debug)]
    struct MockTxManager {
        response: std::sync::Mutex<Option<SendResponse>>,
    }

    impl MockTxManager {
        fn new(response: SendResponse) -> Self {
            Self { response: std::sync::Mutex::new(Some(response)) }
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            self.response.lock().unwrap().take().expect("MockTxManager response already consumed")
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("not needed for these tests")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    // ========================================================================
    // Proof data encoding tests
    // ========================================================================

    #[test]
    fn test_build_proof_data_length() {
        let proposal = test_proposal();
        let proof = build_proof_data(&proposal).unwrap();
        // 1 (type) + 32 (l1OriginHash) + 32 (l1OriginNumber) + 65 (sig) = 130
        assert_eq!(proof.len(), 130);
    }

    #[test]
    fn test_build_proof_data_type_byte() {
        let proposal = test_proposal();
        let proof = build_proof_data(&proposal).unwrap();
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_build_proof_data_v_value_adjustment() {
        let mut proposal = test_proposal();
        let mut sig = proposal.signature.to_vec();
        sig[64] = 0;
        proposal.signature = Bytes::from(sig);

        let proof = build_proof_data(&proposal).unwrap();
        assert_eq!(proof[129], 27);
    }

    #[test]
    fn test_build_proof_data_v_value_already_adjusted() {
        let mut proposal = test_proposal();
        let mut sig = proposal.signature.to_vec();
        sig[64] = 28;
        proposal.signature = Bytes::from(sig);

        let proof = build_proof_data(&proposal).unwrap();
        assert_eq!(proof[129], 28);
    }

    #[test]
    fn test_build_proof_data_v_value_rejects_invalid() {
        let mut proposal = test_proposal();
        let mut sig = proposal.signature.to_vec();
        sig[64] = 5;
        proposal.signature = Bytes::from(sig);

        let result = build_proof_data(&proposal);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid ECDSA v-value"));
    }

    // ========================================================================
    // ProposalSubmitter tests
    // ========================================================================

    #[tokio::test]
    async fn propose_output_success() {
        let tx_hash = B256::repeat_byte(0xAA);
        let mock = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));
        let submitter =
            ProposalSubmitter::new(mock, Address::repeat_byte(0x01), 1, U256::from(100));

        let proposal = test_proposal();
        let result = submitter.propose_output(&proposal, 200, 0, &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn propose_output_reverted() {
        let tx_hash = B256::repeat_byte(0xBB);
        let mock = MockTxManager::new(Ok(receipt_with_status(false, tx_hash)));
        let submitter =
            ProposalSubmitter::new(mock, Address::repeat_byte(0x01), 1, U256::from(100));

        let proposal = test_proposal();
        let err = submitter.propose_output(&proposal, 200, 0, &[]).await.unwrap_err();
        assert!(matches!(err, ProposerError::TxReverted(_)));
    }

    #[tokio::test]
    async fn propose_output_tx_manager_error() {
        let mock = MockTxManager::new(Err(TxManagerError::NonceTooLow));
        let submitter =
            ProposalSubmitter::new(mock, Address::repeat_byte(0x01), 1, U256::from(100));

        let proposal = test_proposal();
        let err = submitter.propose_output(&proposal, 200, 0, &[]).await.unwrap_err();
        assert!(
            matches!(err, ProposerError::TxManager(TxManagerError::NonceTooLow)),
            "expected TxManager(NonceTooLow), got {err:?}",
        );
    }

    #[test]
    fn classify_game_already_exists_by_selector() {
        let hex = GAME_ALREADY_EXISTS_HEX.as_str();
        let err = TxManagerError::Rpc(format!("execution reverted: 0x{hex}"));
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::GameAlreadyExists));
    }

    #[test]
    fn classify_game_already_exists_by_name() {
        let err = TxManagerError::Rpc("GameAlreadyExists()".to_string());
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::GameAlreadyExists));
    }

    #[test]
    fn classify_execution_reverted_with_reason() {
        let err = TxManagerError::ExecutionReverted {
            reason: Some("GameAlreadyExists()".to_string()),
            data: None,
        };
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::GameAlreadyExists));
    }

    #[test]
    fn classify_execution_reverted_with_selector_data() {
        use alloy_primitives::Bytes;
        use base_proof_contracts::game_already_exists_selector;

        // Build data: 4-byte selector + 32 bytes of argument.
        let mut data = game_already_exists_selector().to_vec();
        data.extend_from_slice(&[0u8; 32]);

        let err = TxManagerError::ExecutionReverted { reason: None, data: Some(Bytes::from(data)) };
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::GameAlreadyExists));
    }

    #[test]
    fn classify_execution_reverted_other() {
        use alloy_primitives::Bytes;

        let err = TxManagerError::ExecutionReverted {
            reason: Some("SomeOtherError()".to_string()),
            data: Some(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef])),
        };
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::TxManager(_)));
    }

    #[test]
    fn classify_other_error() {
        let err = TxManagerError::NonceTooLow;
        let result = classify_tx_manager_error(err);
        assert!(matches!(result, ProposerError::TxManager(TxManagerError::NonceTooLow)));
    }

    #[test]
    fn test_is_game_already_exists() {
        let e = ProposerError::GameAlreadyExists;
        assert!(is_game_already_exists(&e));

        let e = ProposerError::Contract("some other error".into());
        assert!(!is_game_already_exists(&e));
    }
}
