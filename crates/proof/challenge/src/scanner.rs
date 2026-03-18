//! Game scanner for the challenger service.
//!
//! Scans the [`DisputeGameFactory`](base_proof_contracts::DisputeGameFactoryClient)
//! for dispute games requiring validation. Each game is evaluated through a
//! two-stage filter:
//!
//! 1. **Status** — must be `IN_PROGRESS` ([`GameScanner::STATUS_IN_PROGRESS`]).
//! 2. **Challenge state** — `zkProver` must be `Address::ZERO` (unchallenged).
//!
//! Games passing both filters are returned as [`CandidateGame`] structs.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DisputeGameFactoryClient, GameAtIndex, GameInfo,
};
use eyre::Result;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::ChallengerMetrics;

/// Configuration for the game scanner.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    /// Number of past games to scan on startup (lookback window).
    pub lookback_games: u64,
}

/// A dispute game that has been identified as a candidate for challenge.
#[derive(Debug, Clone)]
pub struct CandidateGame {
    /// The factory index of this game.
    pub index: u64,
    /// Game data from the factory contract.
    pub factory: GameAtIndex,
    /// Game info from the verifier contract.
    pub info: GameInfo,
    /// The starting block number for this game.
    pub starting_block_number: u64,
    /// The intermediate block interval for this game's type.
    pub intermediate_block_interval: u64,
}

/// Scans the `DisputeGameFactory` for new dispute games that need validation.
///
/// The scanner is stateless across restarts — `last_scanned_index` is ephemeral
/// and recomputed from the lookback window on startup.
pub struct GameScanner {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    config: ScannerConfig,
    /// Cache of `game_type → intermediate_block_interval` to avoid repeated RPC calls.
    interval_cache: Mutex<HashMap<u32, u64>>,
}

impl std::fmt::Debug for GameScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameScanner").field("config", &self.config).finish_non_exhaustive()
    }
}

impl GameScanner {
    /// Game status indicating the dispute is still in progress.
    pub const STATUS_IN_PROGRESS: u8 = 0;

    /// Maximum number of games to evaluate concurrently during a scan.
    pub const SCAN_CONCURRENCY: usize = 32;

    /// Creates a new game scanner.
    pub fn new(
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        config: ScannerConfig,
    ) -> Self {
        Self { factory_client, verifier_client, config, interval_cache: Mutex::new(HashMap::new()) }
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

        let results: Vec<(u64, Result<Option<CandidateGame>>)> = stream::iter(start..=end)
            .map(|i| async move { (i, self.evaluate_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .collect()
            .await;

        let mut candidates = Vec::new();
        let mut lowest_error: Option<u64> = None;

        for (i, result) in results {
            match result {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, index = i, "failed to query game, skipping");
                    lowest_error = lowest_error.map_or(Some(i), |prev| Some(prev.min(i)));
                }
            }
        }

        candidates.sort_by_key(|c| c.index);

        metrics::counter!(ChallengerMetrics::GAMES_SCANNED_TOTAL).increment(games_to_scan);

        let new_last_scanned = match lowest_error {
            Some(0) => last_scanned,
            Some(e) => Some(e - 1),
            None => Some(end),
        };

        if let Some(head) = new_last_scanned {
            metrics::gauge!(ChallengerMetrics::SCAN_HEAD).set(head as f64);
        }

        info!(
            games_found = candidates.len(),
            scan_head = ?new_last_scanned,
            games_scanned = games_to_scan,
            "scan complete"
        );

        Ok((candidates, new_last_scanned))
    }

    /// Evaluates a single game at the given factory index.
    ///
    /// Returns `Some(CandidateGame)` if the game is `IN_PROGRESS` and has not
    /// been challenged (`zkProver` == zero). Returns `None` if the game should
    /// be skipped.
    pub async fn evaluate_game(&self, index: u64) -> Result<Option<CandidateGame>> {
        let factory = self.factory_client.game_at_index(index).await?;

        let status = self.verifier_client.status(factory.proxy).await?;
        if status != Self::STATUS_IN_PROGRESS {
            debug!(index = index, status = status, "skipping game not in progress");
            return Ok(None);
        }

        let zk_prover = self.verifier_client.zk_prover(factory.proxy).await?;
        if zk_prover != Address::ZERO {
            debug!(
                index = index,
                zk_prover = %zk_prover,
                "skipping already-challenged game"
            );
            return Ok(None);
        }

        let (info, starting_block_number) = tokio::try_join!(
            self.verifier_client.game_info(factory.proxy),
            self.verifier_client.starting_block_number(factory.proxy),
        )?;

        let intermediate_block_interval =
            self.resolve_intermediate_block_interval(factory.game_type).await?;

        Ok(Some(CandidateGame {
            index,
            factory,
            info,
            starting_block_number,
            intermediate_block_interval,
        }))
    }

    /// Resolves the intermediate block interval for a game type, using a cache
    /// to avoid repeated RPC calls for the same type.
    async fn resolve_intermediate_block_interval(&self, game_type: u32) -> Result<u64> {
        {
            let cache = self.interval_cache.lock().expect("interval_cache lock poisoned");
            if let Some(&interval) = cache.get(&game_type) {
                return Ok(interval);
            }
        }

        let impl_address = self.factory_client.game_impls(game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no game implementation registered in DisputeGameFactory for game type {game_type}"
            ));
        }

        let interval = self.verifier_client.read_intermediate_block_interval(impl_address).await?;

        debug!(
            game_type = game_type,
            interval = interval,
            impl_address = %impl_address,
            "resolved intermediate block interval"
        );

        let mut cache = self.interval_cache.lock().expect("interval_cache lock poisoned");
        cache.insert(game_type, interval);

        Ok(interval)
    }
}
