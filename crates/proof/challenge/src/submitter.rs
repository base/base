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

    /// Returns the address that will submit transactions on-chain.
    pub fn sender_address(&self) -> Address {
        self.tx_manager.sender_address()
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

        info!(
            game = %game_address,
            intermediate_root_index,
            intermediate_root_to_prove = %intermediate_root_to_prove,
            calldata_len = calldata.len(),
            "Nullifying dispute game"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(game_address),
            value: U256::ZERO,
            ..Default::default()
        };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        ChallengerMetrics::nullify_tx_submitted_total().increment(1);
        let start = Instant::now();
        let result = self.tx_manager.send(candidate).await;
        let latency = start.elapsed();

        let status_label = match &result {
            Ok(receipt) if receipt.inner.status() => ChallengerMetrics::STATUS_SUCCESS,
            Ok(_) => ChallengerMetrics::STATUS_REVERTED,
            Err(_) => ChallengerMetrics::STATUS_ERROR,
        };
        ChallengerMetrics::nullify_tx_outcome_total(status_label).increment(1);
        ChallengerMetrics::nullify_tx_latency_seconds().record(latency.as_secs_f64());

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
    use alloy_primitives::Address;
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::test_utils::{MockTxManager, receipt_with_status};

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
