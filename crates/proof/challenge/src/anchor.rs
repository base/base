//! Anchor root update management.

use std::{
    collections::{HashMap, hash_map::Entry},
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, GameStatus, encode_resolve_calldata, encode_set_anchor_state_calldata,
};
use base_runtime::Clock;
use tracing::{debug, info, warn};

use crate::{BondTransactionSubmitter, ChallengerMetrics};

#[derive(Debug, Clone, Copy)]
enum AnchorUpdateOutcome {
    Pending,
    Retry,
    Complete,
}

/// Best-effort updater for the `AnchorStateRegistry`.
#[derive(Debug)]
pub struct AnchorUpdater<C: Clock> {
    tracked: HashMap<Address, TrackedAnchorUpdate>,
    clock: C,
    retention: Duration,
}

/// Cached state for a tracked anchor update.
#[derive(Debug, Default)]
pub struct TrackedAnchorUpdate {
    /// Monotonic timestamp when retryable failures began.
    pub retry_started_at: Option<Duration>,
    /// Cached terminal game status.
    pub status: Option<GameStatus>,
    /// Cached `AnchorStateRegistry` address for the game.
    pub asr_address: Option<Address>,
    /// Cached immutable game L2 block number.
    pub l2_block_number: Option<u64>,
}

impl<C: Clock> AnchorUpdater<C> {
    /// Creates an anchor updater.
    pub fn new(clock: C, retention: Duration) -> Self {
        info!(retention_secs = retention.as_secs(), "anchor update retention configured");
        Self { tracked: HashMap::new(), clock, retention }
    }

    /// Registers a game for anchor root advancement.
    pub fn track_game(&mut self, game_address: Address) {
        let Entry::Vacant(entry) = self.tracked.entry(game_address) else {
            debug!(game = %game_address, "game already tracked for anchor update");
            return;
        };

        info!(game = %game_address, "tracking game for anchor update");
        entry.insert(TrackedAnchorUpdate::default());
        ChallengerMetrics::anchor_update_tracked_games().set(self.tracked.len() as f64);
    }

    /// Returns `true` if the given game is being tracked.
    pub fn is_tracking(&self, game_address: &Address) -> bool {
        self.tracked.contains_key(game_address)
    }

    /// Polls all tracked games and advances eligible anchor roots.
    pub async fn poll(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) {
        if self.tracked.is_empty() {
            return;
        }

        let addresses: Vec<Address> = self.tracked.keys().copied().collect();

        for game_address in addresses {
            let Some(game) = self.tracked.get_mut(&game_address) else {
                continue;
            };

            let outcome = Self::try_update(game_address, game, verifier_client, submitter).await;
            let should_remove = match outcome {
                AnchorUpdateOutcome::Complete => true,
                AnchorUpdateOutcome::Pending => {
                    if let Some(game) = self.tracked.get_mut(&game_address) {
                        game.retry_started_at = None;
                    }
                    false
                }
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

        let Some(retry_started_at) = game.retry_started_at else {
            game.retry_started_at = Some(now);
            return false;
        };

        let elapsed = now.saturating_sub(retry_started_at);
        if elapsed < self.retention {
            return false;
        }

        warn!(
            game = %game_address,
            retained_secs = elapsed.as_secs(),
            retention_secs = self.retention.as_secs(),
            "dropping anchor update after retention timeout"
        );
        true
    }

    async fn try_update(
        game_address: Address,
        game: &mut TrackedAnchorUpdate,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> AnchorUpdateOutcome {
        let status = if let Some(status) = game.status {
            status
        } else {
            match verifier_client.status(game_address).await {
                Ok(s) => {
                    if s != GameStatus::InProgress {
                        game.status = Some(s);
                    }
                    s
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read status for anchor update, will retry"
                    );
                    return AnchorUpdateOutcome::Retry;
                }
            }
        };

        if status == GameStatus::InProgress {
            let (zk_prover, tee_prover, countered_index) = match tokio::try_join!(
                verifier_client.zk_prover(game_address),
                verifier_client.tee_prover(game_address),
                verifier_client.countered_index(game_address),
            ) {
                Ok(provers) => provers,
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read provers for anchor update, will retry"
                    );
                    return AnchorUpdateOutcome::Retry;
                }
            };

            if tee_prover == Address::ZERO && zk_prover == Address::ZERO {
                debug!(game = %game_address, "skipping fully nullified anchor update");
                ChallengerMetrics::anchor_update_tx_outcome_total(
                    ChallengerMetrics::STATUS_SKIPPED,
                )
                .increment(1);
                return AnchorUpdateOutcome::Complete;
            }

            if countered_index > 0 {
                debug!(
                    game = %game_address,
                    countered_index,
                    "anchor update waiting for active challenge"
                );
                return AnchorUpdateOutcome::Pending;
            }

            let game_over = match verifier_client.game_over(game_address).await {
                Ok(game_over) => game_over,
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read gameOver for anchor update, will retry"
                    );
                    return AnchorUpdateOutcome::Retry;
                }
            };

            if !game_over {
                return AnchorUpdateOutcome::Pending;
            }

            let calldata = encode_resolve_calldata();
            info!(game = %game_address, "submitting resolve transaction for anchor update");
            match submitter.send_bond_tx(game_address, game_address, calldata).await {
                Ok(tx_hash) => {
                    info!(
                        game = %game_address,
                        tx_hash = %tx_hash,
                        "resolve transaction confirmed for anchor update"
                    );
                    ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                        .increment(1);
                    return AnchorUpdateOutcome::Pending;
                }
                Err(e) => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "resolve transaction failed for anchor update, will retry"
                    );
                    ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                        .increment(1);
                    return AnchorUpdateOutcome::Retry;
                }
            }
        }

        if status != GameStatus::DefenderWins {
            ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
                .increment(1);
            return AnchorUpdateOutcome::Complete;
        }

        let asr_address = if let Some(asr_address) = game.asr_address {
            asr_address
        } else {
            match verifier_client.anchor_state_registry(game_address).await {
                Ok(addr) => {
                    game.asr_address = Some(addr);
                    addr
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read anchorStateRegistry for anchor update"
                    );
                    return AnchorUpdateOutcome::Retry;
                }
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
                return AnchorUpdateOutcome::Pending;
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
            return AnchorUpdateOutcome::Pending;
        }

        if !preflight.respected {
            debug!(
                game = %game_address,
                asr = %asr_address,
                "anchor update not ready because game is not currently respected"
            );
            return AnchorUpdateOutcome::Retry;
        }

        let game_l2_block_number = if let Some(l2_block_number) = game.l2_block_number {
            l2_block_number
        } else {
            match verifier_client.game_info(game_address).await {
                Ok(info) => {
                    game.l2_block_number = Some(info.l2_block_number);
                    info.l2_block_number
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        asr = %asr_address,
                        error = %e,
                        "failed to read game info for anchor preflight, will retry"
                    );
                    return AnchorUpdateOutcome::Retry;
                }
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
    use std::{collections::HashMap, time::Duration};

    use alloy_primitives::B256;
    use base_runtime::TokioRuntime;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockBondTransactionSubmitter, addr, mock_state, mock_state_with_tee,
    };

    const DEFAULT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

    #[tokio::test]
    async fn poll_updates_anchor_for_defender_wins() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash)]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, asr);
        assert!(updater.tracked.is_empty());
    }

    #[tokio::test]
    async fn poll_keeps_unfinalized_game_pending() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;
        state.is_finalized = false;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), Duration::ZERO);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(updater.tracked.contains_key(&game));
        assert_eq!(updater.tracked[&game].retry_started_at, None);
    }

    #[tokio::test]
    async fn poll_keeps_paused_registry_pending_until_unpaused() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;
        state.is_paused = true;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state.clone())]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(tx_hash)]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), Duration::ZERO);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(updater.tracked.contains_key(&game));
        assert_eq!(updater.tracked[&game].retry_started_at, None);

        state.is_paused = false;
        verifier.update_game(game, state);

        updater.poll(&verifier, &submitter).await;

        assert_eq!(submitter.recorded_calls().len(), 1);
        assert!(updater.tracked.is_empty());
    }

    #[tokio::test]
    async fn poll_caches_anchor_metadata_while_waiting_for_finality() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;
        state.is_finalized = false;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert!(updater.tracked.contains_key(&game));
        assert_eq!(verifier.status_read_count(game), 1);
        assert_eq!(verifier.anchor_state_registry_read_count(game), 1);
        assert_eq!(verifier.game_info_read_count(game), 0);
    }

    #[tokio::test]
    async fn poll_resolves_game_before_anchor_update() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let resolve_tx_hash = B256::repeat_byte(0xCC);
        let anchor_tx_hash = B256::repeat_byte(0xDD);
        let mut state = mock_state(GameStatus::InProgress, Address::ZERO, 100);
        state.anchor_state_registry = asr;
        state.game_over = true;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state.clone())]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![
            Ok(resolve_tx_hash),
            Ok(anchor_tx_hash),
        ]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, game);
        assert_eq!(calls[0].2, encode_resolve_calldata());
        assert!(updater.tracked.contains_key(&game));

        state.status = GameStatus::DefenderWins;
        verifier.update_game(game, state);

        updater.poll(&verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1, asr);
        assert!(updater.tracked.is_empty());
    }

    #[tokio::test]
    async fn poll_keeps_pre_game_over_game_pending() {
        let game = addr(0);
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);
        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), Duration::ZERO);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(updater.tracked.contains_key(&game));
        assert_eq!(updater.tracked[&game].retry_started_at, None);
    }

    #[tokio::test]
    async fn poll_waits_for_active_challenge() {
        let game = addr(0);
        let tee = Address::repeat_byte(0xEE);
        let zk = Address::repeat_byte(0xCC);
        let mut state = mock_state_with_tee(GameStatus::InProgress, zk, tee, 100);
        state.countered_index = 1;
        state.game_over = true;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), Duration::ZERO);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(updater.tracked.contains_key(&game));
        assert_eq!(updater.tracked[&game].retry_started_at, None);
    }

    #[tokio::test]
    async fn poll_skips_non_defender_wins() {
        let game = addr(0);
        let state = mock_state(GameStatus::ChallengerWins, Address::ZERO, 100);
        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(!updater.tracked.contains_key(&game));
    }

    #[tokio::test]
    async fn poll_caches_game_l2_block_across_retries() {
        let game = addr(0);
        let asr = Address::repeat_byte(0xAA);
        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.anchor_state_registry = asr;

        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![
            Err(crate::ChallengeSubmitError::TxReverted { tx_hash: B256::ZERO }),
            Err(crate::ChallengeSubmitError::TxReverted { tx_hash: B256::ZERO }),
        ]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        updater.poll(&verifier, &submitter).await;

        assert_eq!(submitter.recorded_calls().len(), 2);
        assert_eq!(verifier.game_info_read_count(game), 1);
        assert!(updater.tracked.contains_key(&game));
    }

    #[tokio::test]
    async fn poll_drops_fully_nullified_game() {
        let game = addr(0);
        let state = mock_state_with_tee(GameStatus::InProgress, Address::ZERO, Address::ZERO, 100);
        let verifier = MockAggregateVerifier::new(HashMap::from([(game, state)]));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), DEFAULT_RETENTION);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(!updater.tracked.contains_key(&game));
    }

    #[tokio::test]
    async fn poll_drops_after_retention_when_status_read_keeps_failing() {
        let game = addr(0);
        let verifier = MockAggregateVerifier::new(HashMap::new());
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        let mut updater = AnchorUpdater::new(TokioRuntime::new(), Duration::ZERO);
        updater.track_game(game);

        updater.poll(&verifier, &submitter).await;
        assert!(updater.tracked.contains_key(&game));

        updater.poll(&verifier, &submitter).await;
        assert!(!updater.tracked.contains_key(&game));
    }
}
