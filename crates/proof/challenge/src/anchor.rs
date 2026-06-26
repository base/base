//! Anchor root update management.

use std::{
    collections::{HashMap, hash_map::Entry},
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_contracts::{AggregateVerifierClient, GameStatus, encode_set_anchor_state_calldata};
use base_runtime::Clock;
use base_tx_manager::TxManager;
use tracing::{debug, info, warn};

use crate::{ChallengeSubmitter, ChallengerMetrics};

#[derive(Debug, Clone, Copy)]
enum AnchorUpdateOutcome {
    Pending,
    Retry,
    Complete,
}

/// Best-effort updater for the `AnchorStateRegistry`.
#[derive(Debug)]
pub struct AnchorUpdater<C: Clock> {
    tracked: HashMap<Address, Option<Duration>>,
    clock: C,
}

impl<C: Clock> AnchorUpdater<C> {
    /// Maximum time to keep retrying after a game becomes eligible for anchor update work.
    const DEFAULT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

    /// Creates an anchor updater.
    pub fn new(clock: C) -> Self {
        Self { tracked: HashMap::new(), clock }
    }

    /// Registers a game for anchor root advancement.
    pub fn track_game(&mut self, game_address: Address) {
        let Entry::Vacant(entry) = self.tracked.entry(game_address) else {
            debug!(game = %game_address, "game already tracked for anchor update");
            return;
        };

        info!(game = %game_address, "tracking game for anchor update");
        entry.insert(None);
        ChallengerMetrics::anchor_update_tracked_games().set(self.tracked.len() as f64);
    }

    /// Polls all tracked games and advances eligible anchor roots.
    pub async fn poll<T: TxManager>(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) {
        if self.tracked.is_empty() {
            return;
        }

        let addresses: Vec<Address> = self.tracked.keys().copied().collect();

        for game_address in addresses {
            let should_remove =
                match Self::try_update(game_address, verifier_client, submitter).await {
                    AnchorUpdateOutcome::Complete => true,
                    AnchorUpdateOutcome::Pending => false,
                    AnchorUpdateOutcome::Retry => self.handle_retry(game_address),
                };

            if should_remove {
                self.tracked.remove(&game_address);
            }
        }

        ChallengerMetrics::anchor_update_tracked_games().set(self.tracked.len() as f64);
    }

    fn handle_retry(&mut self, game_address: Address) -> bool {
        let now = self.clock.now();
        let Some(game) = self.tracked.get_mut(&game_address) else {
            return false;
        };

        let Some(retry_started_at) = *game else {
            *game = Some(now);
            return false;
        };

        let elapsed = now.saturating_sub(retry_started_at);
        if elapsed < Self::DEFAULT_RETENTION {
            return false;
        }

        warn!(
            game = %game_address,
            retained_secs = elapsed.as_secs(),
            retention_secs = Self::DEFAULT_RETENTION.as_secs(),
            "dropping anchor update after retention timeout"
        );
        true
    }

    async fn try_update<T: TxManager>(
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> AnchorUpdateOutcome {
        let status = match verifier_client.status(game_address).await {
            Ok(s) => s,
            Err(e) => {
                debug!(game = %game_address, error = %e, "failed to read status for anchor update");
                return AnchorUpdateOutcome::Pending;
            }
        };

        if status == GameStatus::InProgress {
            return AnchorUpdateOutcome::Pending;
        }

        if status != GameStatus::DefenderWins {
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return AnchorUpdateOutcome::Complete;
        }

        let asr_address = match verifier_client.anchor_state_registry(game_address).await {
            Ok(addr) => addr,
            Err(e) => {
                debug!(
                    game = %game_address,
                    error = %e,
                    "failed to read anchorStateRegistry for anchor update"
                );
                return AnchorUpdateOutcome::Retry;
            }
        };

        match verifier_client.is_game_finalized(asr_address, game_address).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    "anchor update not ready because game is not finalized"
                );
                return AnchorUpdateOutcome::Retry;
            }
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "failed to read isGameFinalized, will retry"
                );
                return AnchorUpdateOutcome::Retry;
            }
        }

        let preflight = match verifier_client.anchor_preflight(asr_address, game_address).await {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "failed to read anchor preflight state, will retry"
                );
                return AnchorUpdateOutcome::Retry;
            }
        };

        if preflight.permanently_ineligible() {
            info!(
                game = %game_address,
                asr = %asr_address,
                blacklisted = preflight.blacklisted,
                retired = preflight.retired,
                respected = preflight.respected,
                "skipping permanently ineligible anchor update"
            );
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return AnchorUpdateOutcome::Complete;
        }

        if preflight.paused {
            debug!(
                game = %game_address,
                asr = %asr_address,
                "anchor update not ready because registry is paused"
            );
            return AnchorUpdateOutcome::Retry;
        }

        if !preflight.respected {
            debug!(
                game = %game_address,
                asr = %asr_address,
                "anchor update not ready because game is not currently respected"
            );
            return AnchorUpdateOutcome::Retry;
        }

        let game_l2_block_number = match verifier_client.game_info(game_address).await {
            Ok(info) => info.l2_block_number,
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "failed to read game info for anchor preflight, will retry"
                );
                return AnchorUpdateOutcome::Retry;
            }
        };

        if game_l2_block_number <= preflight.anchor_root.l2_block_number {
            info!(
                game = %game_address,
                asr = %asr_address,
                game_l2_block = game_l2_block_number,
                anchor_l2_block = preflight.anchor_root.l2_block_number,
                "skipping stale anchor update"
            );
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return AnchorUpdateOutcome::Complete;
        }

        let calldata = encode_set_anchor_state_calldata(game_address);
        match submitter.send_bond_tx(game_address, asr_address, calldata).await {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    asr = %asr_address,
                    tx_hash = %tx_hash,
                    "anchor state registry updated"
                );
                ChallengerMetrics::anchor_update_tx_outcome_total(
                    ChallengerMetrics::STATUS_SUCCESS,
                )
                .increment(1);
                // Prometheus gauges are f64; this is telemetry, not protocol state.
                ChallengerMetrics::anchor_l2_block_number().set(game_l2_block_number as f64);
                AnchorUpdateOutcome::Complete
            }
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "anchor update failed, will retry"
                );
                ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                AnchorUpdateOutcome::Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::B256;
    use base_runtime::TokioRuntime;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockTxManager, addr, mock_state, receipt_with_status,
    };

    fn submitter(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (ChallengeSubmitter<MockTxManager>, MockTxManager) {
        let tx_manager = MockTxManager::with_responses(responses);
        (ChallengeSubmitter::new(tx_manager.clone()), tx_manager)
    }

    #[tokio::test]
    async fn poll_updates_anchor_for_defender_wins() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![Ok(receipt_with_status(true, tx_hash))]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new());
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(asr));
        assert!(updater.tracked.is_empty());
    }

    #[tokio::test]
    async fn poll_retries_when_game_is_not_finalized() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;
        state.is_finalized = false;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new());
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        assert!(tx_manager.recorded_calls().is_empty());
        assert!(updater.tracked.contains_key(&game));
    }

    #[tokio::test]
    async fn poll_skips_non_defender_wins() {
        let game = addr(0);
        let state = mock_state(GameStatus::ChallengerWins, Address::ZERO, 100);
        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let (submitter, tx_manager) = submitter(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new());
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        assert!(tx_manager.recorded_calls().is_empty());
        assert!(!updater.tracked.contains_key(&game));
    }
}
