//! Challenge submission logic for disputing invalid dispute games.
//!
//! Three dispute paths are supported:
//!
//! 1. **Wrong TEE proof** — `nullify()` with a TEE proof, or `challenge()`
//!    with a ZK proof.
//! 2. **Correct TEE proof challenged with a wrong ZK proof** — `nullify()`
//!    with a ZK proof to refute the fraudulent challenge.
//! 3. **Wrong ZK proposal** — `nullify()` with a ZK proof.

use std::time::Instant;

use alloy_primitives::{Address, B256, Bytes, U256};
use base_proof_contracts::{encode_challenge_calldata, encode_nullify_calldata};
use base_tx_manager::{TxCandidate, TxManager};
use tracing::{debug, info};

use crate::{BondTransactionSubmitter, ChallengeSubmitError, ChallengerMetrics, DisputeIntent};

/// Submits dispute transactions (nullify or challenge) to game contracts on L1.
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

    /// Submits a dispute transaction (`nullify()` or `challenge()`) to the
    /// given dispute game contract, based on the [`DisputeIntent`].
    ///
    /// - [`DisputeIntent::Nullify`] — used across all three dispute paths:
    ///   TEE nullification of a wrong TEE proof (Path 1), ZK nullification
    ///   of a fraudulent challenge (Path 2), and ZK nullification of a wrong
    ///   ZK proposal (Path 3).
    /// - [`DisputeIntent::Challenge`] — challenges a TEE proof with a
    ///   competing ZK proof (Path 1). The game resolves as `CHALLENGER_WINS`
    ///   if the challenge is not nullified.
    ///
    /// Returns the transaction hash on success, or an error if the transaction
    /// manager fails or the transaction reverts on-chain.
    pub async fn submit_dispute(
        &self,
        game_address: Address,
        proof_bytes: Bytes,
        intermediate_root_index: u64,
        intermediate_root_to_prove: B256,
        intent: DisputeIntent,
    ) -> Result<B256, ChallengeSubmitError> {
        let calldata = match intent {
            DisputeIntent::Nullify => encode_nullify_calldata(
                proof_bytes,
                intermediate_root_index,
                intermediate_root_to_prove,
            ),
            DisputeIntent::Challenge => encode_challenge_calldata(
                proof_bytes,
                intermediate_root_index,
                intermediate_root_to_prove,
            ),
        };

        info!(
            game = %game_address,
            action = intent.label(),
            intermediate_root_index,
            intermediate_root_to_prove = %intermediate_root_to_prove,
            calldata_len = calldata.len(),
            "submitting dispute transaction"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(game_address),
            value: U256::ZERO,
            ..Default::default()
        };

        debug!(
            tx = ?candidate,
            "sending tx candidate",
        );

        intent.record_submitted();
        let start = Instant::now();
        let result = self.tx_manager.send(candidate).await;
        let latency = start.elapsed();

        let (status_label, succeeded) = match &result {
            Ok(receipt) if receipt.inner.status() => (ChallengerMetrics::STATUS_SUCCESS, true),
            Ok(_) => (ChallengerMetrics::STATUS_REVERTED, false),
            Err(_) => (ChallengerMetrics::STATUS_ERROR, false),
        };
        intent.record_outcome(status_label);
        intent.record_latency(latency.as_secs_f64());

        let receipt = result?;
        let tx_hash = receipt.transaction_hash;

        if !succeeded {
            return Err(ChallengeSubmitError::TxReverted { tx_hash });
        }

        info!(tx_hash = %tx_hash, game = %game_address, action = intent.label(), "dispute transaction confirmed");

        Ok(tx_hash)
    }
}

#[async_trait::async_trait]
impl<T: TxManager> BondTransactionSubmitter for ChallengeSubmitter<T> {
    async fn send_bond_tx(
        &self,
        game_address: Address,
        to: Address,
        calldata: Bytes,
    ) -> Result<B256, ChallengeSubmitError> {
        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(to),
            value: U256::ZERO,
            ..Default::default()
        };

        info!(
            game = %game_address,
            to = %to,
            calldata_len = candidate.tx_data.len(),
            "sending bond transaction"
        );

        let start = Instant::now();
        let receipt = self.tx_manager.send(candidate).await?;
        let latency = start.elapsed();
        ChallengerMetrics::bond_tx_latency_seconds().record(latency.as_secs_f64());
        let tx_hash = receipt.transaction_hash;

        if !receipt.inner.status() {
            return Err(ChallengeSubmitError::TxReverted { tx_hash });
        }

        Ok(tx_hash)
    }
}

/// Metrics recording helpers for [`DisputeIntent`].
impl DisputeIntent {
    /// Returns a human-readable label for logging and metrics.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Nullify => "nullify",
            Self::Challenge => "challenge",
        }
    }

    /// Records that a dispute transaction has been submitted.
    pub fn record_submitted(&self) {
        match self {
            Self::Nullify => ChallengerMetrics::nullify_tx_submitted_total().increment(1),
            Self::Challenge => ChallengerMetrics::challenge_tx_submitted_total().increment(1),
        }
    }

    /// Records the outcome (success, reverted, error) of a dispute transaction.
    pub fn record_outcome(&self, status: &'static str) {
        match self {
            Self::Nullify => ChallengerMetrics::nullify_tx_outcome_total(status).increment(1),
            Self::Challenge => ChallengerMetrics::challenge_tx_outcome_total(status).increment(1),
        }
    }

    /// Records the latency of a dispute transaction.
    pub fn record_latency(&self, seconds: f64) {
        match self {
            Self::Nullify => ChallengerMetrics::nullify_tx_latency_seconds().record(seconds),
            Self::Challenge => ChallengerMetrics::challenge_tx_latency_seconds().record(seconds),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::test_utils::{MockTxManager, receipt_with_status};

    #[tokio::test]
    async fn submit_dispute_nullify_success_returns_tx_hash() {
        let tx_hash = B256::repeat_byte(0xAA);
        let mock = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_dispute(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x00, 0x01]),
                42,
                B256::repeat_byte(0xFF),
                DisputeIntent::Nullify,
            )
            .await;

        assert_eq!(result.unwrap(), tx_hash);
    }

    #[tokio::test]
    async fn submit_dispute_reverted_returns_error() {
        let tx_hash = B256::repeat_byte(0xBB);
        let mock = MockTxManager::new(Ok(receipt_with_status(false, tx_hash)));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_dispute(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x00]),
                1,
                B256::ZERO,
                DisputeIntent::Nullify,
            )
            .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, ChallengeSubmitError::TxReverted { tx_hash: h } if h == tx_hash),
            "expected TxReverted, got {err:?}",
        );
    }

    #[tokio::test]
    async fn submit_dispute_tx_manager_error_propagates() {
        let mock = MockTxManager::new(Err(TxManagerError::NonceTooLow));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_dispute(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x01]),
                0,
                B256::ZERO,
                DisputeIntent::Challenge,
            )
            .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, ChallengeSubmitError::TxManager(TxManagerError::NonceTooLow)),
            "expected TxManager(NonceTooLow), got {err:?}",
        );
    }

    #[tokio::test]
    async fn submit_dispute_challenge_success_returns_tx_hash() {
        let tx_hash = B256::repeat_byte(0xCC);
        let mock = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));
        let submitter = ChallengeSubmitter::new(mock);

        let result = submitter
            .submit_dispute(
                Address::repeat_byte(0x01),
                Bytes::from(vec![0x01, 0xDE, 0xAD]),
                5,
                B256::repeat_byte(0xAB),
                DisputeIntent::Challenge,
            )
            .await;

        assert_eq!(result.unwrap(), tx_hash);
    }
}
