//! Challenge submission logic for nullifying invalid dispute games.

use std::time::Instant;

use alloy_primitives::{Address, B256, Bytes, U256};
use base_proof_contracts::encode_nullify_calldata;
use base_tx_manager::{TxCandidate, TxManager};
use tracing::info;

use crate::{ChallengeSubmitError, ChallengerMetrics};

/// Submits nullify transactions to dispute game contracts on L1.
#[derive(Debug)]
pub struct ChallengeSubmitter<T> {
    tx_manager: T,
}

impl<T: TxManager> ChallengeSubmitter<T> {
    /// Creates a new [`ChallengeSubmitter`] backed by the given transaction manager.
    pub const fn new(tx_manager: T) -> Self {
        Self { tx_manager }
    }

    /// Submits a `nullify()` transaction to the given dispute game contract.
    ///
    /// Returns the transaction hash on success, or an error if the transaction
    /// manager fails or the transaction reverts on-chain.
    pub async fn submit_nullification(
        &self,
        game_address: Address,
        proof_bytes: Bytes,
        intermediate_root_index: u64,
        intermediate_root_to_prove: B256,
    ) -> Result<B256, ChallengeSubmitError> {
        let calldata = encode_nullify_calldata(
            proof_bytes,
            intermediate_root_index,
            intermediate_root_to_prove,
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(game_address),
            value: U256::ZERO,
            ..Default::default()
        };

        metrics::counter!(ChallengerMetrics::NULLIFY_TX_SUBMITTED_TOTAL).increment(1);
        let start = Instant::now();
        let result = self.tx_manager.send(candidate).await;
        let latency = start.elapsed();

        let status_label = match &result {
            Ok(receipt) if receipt.inner.status() => ChallengerMetrics::STATUS_SUCCESS,
            Ok(_) => ChallengerMetrics::STATUS_REVERTED,
            Err(_) => ChallengerMetrics::STATUS_ERROR,
        };
        metrics::counter!(
            ChallengerMetrics::NULLIFY_TX_OUTCOME_TOTAL,
            ChallengerMetrics::LABEL_STATUS => status_label,
        )
        .increment(1);
        metrics::histogram!(ChallengerMetrics::NULLIFY_TX_LATENCY_SECONDS)
            .record(latency.as_secs_f64());

        let receipt = result?;
        let tx_hash = receipt.transaction_hash;

        if !receipt.inner.status() {
            return Err(ChallengeSubmitError::TxReverted { tx_hash });
        }

        info!(tx_hash = %tx_hash, game = %game_address, "nullify transaction confirmed");

        Ok(tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_tx_manager::{SendHandle, SendResponse, TxManagerError};

    use super::*;

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

    #[tokio::test]
    async fn submit_nullification_success_returns_tx_hash() {
        let tx_hash = B256::repeat_byte(0xAA);
        let mock = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_nullification(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x00, 0x01]),
                42,
                B256::repeat_byte(0xFF),
            )
            .await;

        assert_eq!(result.unwrap(), tx_hash);
    }

    #[tokio::test]
    async fn submit_nullification_reverted_returns_error() {
        let tx_hash = B256::repeat_byte(0xBB);
        let mock = MockTxManager::new(Ok(receipt_with_status(false, tx_hash)));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_nullification(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x00]),
                1,
                B256::ZERO,
            )
            .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, ChallengeSubmitError::TxReverted { tx_hash: h } if h == tx_hash),
            "expected TxReverted, got {err:?}",
        );
    }

    #[tokio::test]
    async fn submit_nullification_tx_manager_error_propagates() {
        let mock = MockTxManager::new(Err(TxManagerError::NonceTooLow));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_nullification(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x01]),
                0,
                B256::ZERO,
            )
            .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, ChallengeSubmitError::TxManager(TxManagerError::NonceTooLow)),
            "expected TxManager(NonceTooLow), got {err:?}",
        );
    }
}
