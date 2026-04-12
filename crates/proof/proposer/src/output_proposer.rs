//! `OutputProposer` trait and `ProposalSubmitter` implementation for L1 transaction submission.
//!
//! Submits output proposals by creating new dispute games via `DisputeGameFactory.create()`.
//! Delegates all transaction lifecycle management (nonce, fees, signing, resubmission)
//! to the shared [`TxManager`].

use std::sync::LazyLock;

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
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

/// Trait for submitting output proposals to L1 via dispute game creation.
#[async_trait]
pub trait OutputProposer: Send + Sync {
    /// Creates a new dispute game for the given proposal.
    async fn propose_output(
        &self,
        proposal: &Proposal,
        l2_block_number: u64,
        parent_address: Address,
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
        parent_address: Address,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        info!(
            l2_block_number,
            parent_address = %parent_address,
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
        parent_address: Address,
        intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        let proof_data =
            proposal.build_proof_data().map_err(|e| ProposerError::Internal(e.to_string()))?;
        let extra_data = encode_extra_data(l2_block_number, parent_address, intermediate_roots);
        let calldata =
            encode_create_calldata(self.game_type, proposal.output_root, extra_data, proof_data);

        info!(
            l2_block_number,
            factory = %self.factory_address,
            game_type = self.game_type,
            calldata = %calldata,
            parent_address = %parent_address,
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
    use alloy_primitives::{Address, Bloom, Bytes};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_proof_primitives::PROOF_TYPE_TEE;
    use base_tx_manager::{SendHandle, SendResponse, TxManagerError};
    use rstest::rstest;

    use super::*;

    /// The expected length of encoded proof data:
    /// 1 (type) + 32 (l1OriginHash) + 32 (l1OriginNumber) + 65 (sig) = 130.
    const EXPECTED_PROOF_DATA_LEN: usize = 130;

    /// Index of the v-value byte within the encoded proof data.
    const V_VALUE_BYTE_INDEX: usize = EXPECTED_PROOF_DATA_LEN - 1;

    /// Test game type for `ProposalSubmitter` tests.
    const TEST_GAME_TYPE: u32 = 1;

    /// Test init bond value.
    const TEST_INIT_BOND: u64 = 100;

    /// Test L2 block number used in proposal tests.
    const TEST_L2_BLOCK: u64 = 200;

    fn test_proposal() -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(0x01),
            signature: {
                let mut sig = vec![0xab; 65];
                sig[64] = 1;
                Bytes::from(sig)
            },
            l1_origin_hash: B256::repeat_byte(0x02),
            l1_origin_number: 300,
            l2_block_number: TEST_L2_BLOCK,
            prev_output_root: B256::repeat_byte(0x03),
            config_hash: B256::repeat_byte(0x04),
        }
    }

    fn proposal_with_v(v: u8) -> Proposal {
        let mut proposal = test_proposal();
        let mut sig = proposal.signature.to_vec();
        sig[64] = v;
        proposal.signature = Bytes::from(sig);
        proposal
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

    fn test_submitter(response: SendResponse) -> ProposalSubmitter<MockTxManager> {
        ProposalSubmitter::new(
            MockTxManager::new(response),
            Address::repeat_byte(0x01),
            TEST_GAME_TYPE,
            U256::from(TEST_INIT_BOND),
        )
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
        let proof = test_proposal().build_proof_data().unwrap();
        assert_eq!(proof.len(), EXPECTED_PROOF_DATA_LEN);
    }

    #[test]
    fn test_build_proof_data_type_byte() {
        let proof = test_proposal().build_proof_data().unwrap();
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[rstest]
    #[case::v_zero_adjusted_to_27(0, Some(27))]
    #[case::v_one_adjusted_to_28(1, Some(28))]
    #[case::v_27_unchanged(27, Some(27))]
    #[case::v_28_unchanged(28, Some(28))]
    #[case::v_5_rejected(5, None)]
    fn test_build_proof_data_v_value(#[case] v_input: u8, #[case] expected: Option<u8>) {
        let proposal = proposal_with_v(v_input);
        let result = proposal.build_proof_data();

        match expected {
            Some(v) => {
                let proof = result.unwrap();
                assert_eq!(proof[V_VALUE_BYTE_INDEX], v);
            }
            None => {
                assert!(result.is_err());
                assert!(
                    result.unwrap_err().to_string().contains("invalid ECDSA v-value"),
                    "expected 'invalid ECDSA v-value' error"
                );
            }
        }
    }

    // ========================================================================
    // ProposalSubmitter tests
    // ========================================================================

    #[tokio::test]
    async fn propose_output_success() {
        let tx_hash = B256::repeat_byte(0xAA);
        let submitter = test_submitter(Ok(receipt_with_status(true, tx_hash)));
        let result =
            submitter.propose_output(&test_proposal(), TEST_L2_BLOCK, Address::ZERO, &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn propose_output_reverted() {
        let tx_hash = B256::repeat_byte(0xBB);
        let submitter = test_submitter(Ok(receipt_with_status(false, tx_hash)));
        let err = submitter
            .propose_output(&test_proposal(), TEST_L2_BLOCK, Address::ZERO, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ProposerError::TxReverted(_)));
    }

    #[tokio::test]
    async fn propose_output_tx_manager_error() {
        let submitter = test_submitter(Err(TxManagerError::NonceTooLow));
        let err = submitter
            .propose_output(&test_proposal(), TEST_L2_BLOCK, Address::ZERO, &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProposerError::TxManager(TxManagerError::NonceTooLow)),
            "expected TxManager(NonceTooLow), got {err:?}",
        );
    }

    // ========================================================================
    // classify_tx_manager_error tests
    // ========================================================================

    #[rstest]
    #[case::rpc_with_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", *GAME_ALREADY_EXISTS_HEX)),
        true,
        "selector hex in Rpc message"
    )]
    #[case::rpc_with_name(
        TxManagerError::Rpc("GameAlreadyExists()".to_string()),
        true,
        "error name in Rpc message"
    )]
    #[case::reverted_with_reason(
        TxManagerError::ExecutionReverted {
            reason: Some("GameAlreadyExists()".to_string()),
            data: None,
        },
        true,
        "reason string contains name"
    )]
    #[case::reverted_with_selector_data(
        {
            let mut data = base_proof_contracts::game_already_exists_selector().to_vec();
            data.extend_from_slice(&[0u8; 32]);
            TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(data)),
            }
        },
        true,
        "raw data contains selector"
    )]
    #[case::reverted_other_error(
        TxManagerError::ExecutionReverted {
            reason: Some("SomeOtherError()".to_string()),
            data: Some(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef])),
        },
        false,
        "unrelated revert"
    )]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, false, "non-revert error")]
    fn test_classify_tx_manager_error(
        #[case] err: TxManagerError,
        #[case] expect_game_exists: bool,
        #[case] scenario: &str,
    ) {
        let result = classify_tx_manager_error(err);
        if expect_game_exists {
            assert!(
                matches!(result, ProposerError::GameAlreadyExists),
                "{scenario}: expected GameAlreadyExists, got {result:?}"
            );
        } else {
            assert!(
                matches!(result, ProposerError::TxManager(_)),
                "{scenario}: expected TxManager, got {result:?}"
            );
        }
    }

    #[rstest]
    #[case::game_already_exists(ProposerError::GameAlreadyExists, true)]
    #[case::other_error(ProposerError::Contract("other".into()), false)]
    fn test_is_game_already_exists(#[case] err: ProposerError, #[case] expected: bool) {
        assert_eq!(err.is_game_already_exists(), expected);
    }
}
