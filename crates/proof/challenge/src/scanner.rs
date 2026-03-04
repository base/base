//! Game scanner for the challenger service.
//!
//! Scans the [`DisputeGameFactory`](base_proof_contracts::DisputeGameFactoryClient)
//! for dispute games requiring validation. Each game is evaluated through a
//! three-stage filter:
//!
//! 1. **Game type** — must match [`ScannerConfig::game_type`].
//! 2. **Status** — must be `IN_PROGRESS` ([`STATUS_IN_PROGRESS`]).
//! 3. **Challenge state** — `zkProver` must be `Address::ZERO` (unchallenged).
//!
//! Games passing all three filters are returned as [`CandidateGame`] structs.

use std::sync::Arc;

use alloy_primitives::{Address, B256};
use base_proof_contracts::{
    AggregateVerifierClient, DisputeGameFactoryClient, GameAtIndex, GameInfo,
};
use eyre::Result;
use tracing::{debug, info, warn};

use crate::ChallengerMetrics;

/// Game status indicating the dispute is still in progress.
pub const STATUS_IN_PROGRESS: u8 = 0;

/// Configuration for the game scanner.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    /// Game type ID to filter for.
    pub game_type: u32,
    /// Number of past games to scan on startup (lookback window).
    pub lookback_games: u64,
}

/// A dispute game that has been identified as a candidate for challenge.
#[derive(Debug, Clone)]
pub struct CandidateGame {
    /// The factory index of this game.
    pub index: u64,
    /// The proxy address of the game contract.
    pub proxy: Address,
    /// The output root claimed by this game.
    pub root_claim: B256,
    /// The L2 block number of this game.
    pub l2_block_number: u64,
    /// The parent game's factory index.
    pub parent_index: u32,
    /// The starting block number for this game.
    pub starting_block_number: u64,
}

/// Scans the `DisputeGameFactory` for new dispute games that need validation.
///
/// The scanner is stateless across restarts — `last_scanned_index` is ephemeral
/// and recomputed from the lookback window on startup.
pub struct GameScanner {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    config: ScannerConfig,
}

impl std::fmt::Debug for GameScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameScanner").field("config", &self.config).finish_non_exhaustive()
    }
}

impl GameScanner {
    /// Creates a new game scanner.
    pub fn new(
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        config: ScannerConfig,
    ) -> Self {
        Self { factory_client, verifier_client, config }
    }

    /// Scans for new candidate games since `last_scanned`.
    ///
    /// Returns a tuple of `(candidates, new_last_scanned)` where `new_last_scanned`
    /// is the latest factory index that was evaluated. The caller is responsible
    /// for tracking `last_scanned` between calls.
    ///
    /// On a fresh start, pass `None` as `last_scanned` and the lookback window will
    /// determine the scan range.
    ///
    /// Individual game query failures are logged and skipped so that a transient
    /// RPC error on one game does not abort the entire scan. After evaluation,
    /// the `base_challenger_games_scanned_total` counter and
    /// `base_challenger_scan_head` gauge are updated.
    pub async fn scan(
        &self,
        last_scanned: Option<u64>,
    ) -> Result<(Vec<CandidateGame>, Option<u64>)> {
        let game_count = self.factory_client.game_count().await?;

        if game_count == 0 {
            debug!("factory has no games");
            return Ok((vec![], last_scanned));
        }

        let end = game_count - 1;
        let lookback_start = game_count.saturating_sub(self.config.lookback_games);
        let start =
            last_scanned.map_or(lookback_start, |idx| idx.saturating_add(1).max(lookback_start));

        if start > end {
            debug!(
                last_scanned = ?last_scanned,
                game_count = game_count,
                "no new games since last scan"
            );
            return Ok((vec![], last_scanned));
        }

        let games_to_scan = end - start + 1;
        let mut candidates = Vec::new();
        let mut lowest_error: Option<u64> = None;

        for i in start..=end {
            match self.evaluate_game(i).await {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, index = i, "failed to query game, skipping");
                    lowest_error = lowest_error.map_or(Some(i), |prev| Some(prev.min(i)));
                }
            }
        }

        metrics::counter!(ChallengerMetrics::GAMES_SCANNED_TOTAL).increment(games_to_scan);
        metrics::gauge!(ChallengerMetrics::SCAN_HEAD).set(end as f64);

        info!(
            games_found = candidates.len(),
            scan_head = end,
            games_scanned = games_to_scan,
            "scan complete"
        );

        let new_last_scanned = match lowest_error {
            Some(0) => last_scanned,
            Some(e) => Some(e - 1),
            None => Some(end),
        };
        Ok((candidates, new_last_scanned))
    }

    /// Evaluates a single game at the given factory index.
    ///
    /// Returns `Some(CandidateGame)` if the game matches the configured game type,
    /// is `IN_PROGRESS`, and has not been challenged (`zkProver` == zero).
    /// Returns `None` if the game should be skipped.
    async fn evaluate_game(&self, index: u64) -> Result<Option<CandidateGame>> {
        let GameAtIndex { game_type, proxy, .. } = self.factory_client.game_at_index(index).await?;

        if game_type != self.config.game_type {
            debug!(
                index = index,
                game_type = game_type,
                expected = self.config.game_type,
                "skipping non-matching game type"
            );
            return Ok(None);
        }

        let status = self.verifier_client.status(proxy).await?;
        if status != STATUS_IN_PROGRESS {
            debug!(index = index, status = status, "skipping game not in progress");
            return Ok(None);
        }

        let zk_prover = self.verifier_client.zk_prover(proxy).await?;
        if zk_prover != Address::ZERO {
            debug!(
                index = index,
                zk_prover = %zk_prover,
                "skipping already-challenged game"
            );
            return Ok(None);
        }

        let (GameInfo { root_claim, l2_block_number, parent_index }, starting_block_number) = tokio::try_join!(
            self.verifier_client.game_info(proxy),
            self.verifier_client.starting_block_number(proxy),
        )?;

        Ok(Some(CandidateGame {
            index,
            proxy,
            root_claim,
            l2_block_number,
            parent_index,
            starting_block_number,
        }))
    }
}
