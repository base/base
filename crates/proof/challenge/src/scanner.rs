//! Game scanner for the challenger service.
//!
//! Scans the [`DisputeGameFactory`](base_proof_contracts::DisputeGameFactoryClient)
//! for dispute games that require action. Each game is classified into one
//! of four [`GameCategory`] variants based on its on-chain state:
//!
//! 1. **[`InvalidTeeProposal`](GameCategory::InvalidTeeProposal)** —
//!    TEE-proposed game (`teeProver != 0`, `zkProver == 0`). The driver
//!    validates the intermediate roots and, if invalid, nullifies with a
//!    TEE proof or challenges with a ZK proof.
//!
//! 2. **[`FraudulentZkChallenge`](GameCategory::FraudulentZkChallenge)** —
//!    A TEE-proposed game that has been challenged by a ZK proof
//!    (`teeProver != 0`, `zkProver != 0`, `counteredByIntermediateRootIndexPlusOne > 0`).
//!    The driver validates the originally proposed root at the challenged
//!    index and, if the original was correct, nullifies the ZK challenge
//!    with a ZK proof.
//!
//! 3. **[`InvalidZkProposal`](GameCategory::InvalidZkProposal)** —
//!    ZK-proposed game (`teeProver == 0`, `zkProver != 0`, unchallenged).
//!    The driver validates the intermediate roots and, if invalid,
//!    nullifies with a ZK proof.
//!
//! 4. **[`InvalidDualProposal`](GameCategory::InvalidDualProposal)** —
//!    Both TEE and ZK proofs are present but no challenge has been filed
//!    (`counteredByIntermediateRootIndexPlusOne == 0`). The driver
//!    nullifies the TEE proof first (fast, synchronous) and falls back to
//!    ZK nullification if TEE proving is unavailable. After the TEE proof
//!    is nullified, the subsequent scan reclassifies the game as
//!    [`InvalidZkProposal`](GameCategory::InvalidZkProposal).
//!
//! Games that are not `IN_PROGRESS` or have been fully nullified (both
//! provers zero) are skipped.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256};
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

/// Classifies why a game was selected as a candidate and what action the
/// driver should take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameCategory {
    /// Path 1: TEE-proposed game with a potentially wrong output root.
    ///
    /// The driver validates the intermediate roots. If invalid it either
    /// nullifies with a TEE proof or challenges with a ZK proof.
    InvalidTeeProposal,

    /// Path 2: A TEE-proposed game was challenged with a potentially
    /// fraudulent ZK proof.
    ///
    /// The driver validates the originally proposed root at the challenged
    /// index. If the original root was actually correct, a ZK proof is
    /// submitted via `nullify()` to refute the challenge.
    FraudulentZkChallenge {
        /// The 0-based index of the challenged intermediate root.
        challenged_index: u64,
    },

    /// Path 3: ZK-proposed game with a potentially wrong output root.
    ///
    /// The driver validates the intermediate roots. If invalid it submits
    /// a ZK proof via `nullify()` to nullify the incorrect ZK proposal.
    InvalidZkProposal,

    /// Path 4: Both TEE and ZK proofs present with no challenge
    /// (`countered_index == 0`). The second proof was added via
    /// `verifyProposalProof`, not via `challenge`.
    ///
    /// Both proofs may still verify an incorrect root. The driver
    /// nullifies the TEE proof first (fast, synchronous) and falls back
    /// to ZK nullification if TEE proving is unavailable or fails.
    /// After TEE nullification the game becomes `(false, true, 0)` and
    /// will be re-classified as [`InvalidZkProposal`] on the next scan.
    InvalidDualProposal,
}

/// A dispute game that has been identified as a candidate for action.
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
    /// The L1 head block hash stored at game creation time.
    pub l1_head: B256,
    /// Address of the TEE prover for this game (`Address::ZERO` if none registered).
    pub tee_prover: Address,
    /// Classification of this candidate and the action the driver should take.
    pub category: GameCategory,
}

impl CandidateGame {
    /// Computes the starting block number for the given intermediate root index.
    pub fn checkpoint_start_block(&self, index: u64) -> eyre::Result<u64> {
        let offset = self
            .intermediate_block_interval
            .checked_mul(index)
            .ok_or_else(|| eyre::eyre!("checkpoint offset overflow"))?;
        self.starting_block_number
            .checked_add(offset)
            .ok_or_else(|| eyre::eyre!("checkpoint start block overflow"))
    }
}

/// Scans the `DisputeGameFactory` for dispute games that need validation.
///
/// The scanner is fully stateless — every call re-evaluates the entire
/// lookback window so that on-chain state changes (new proofs added,
/// challenges filed) are always detected.
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

    /// Scans the lookback window for candidate games that need validation.
    ///
    /// Every call re-evaluates the full lookback window so that on-chain state
    /// changes (new proofs added, challenges filed) are always detected. Games
    /// that are not `IN_PROGRESS` or have been fully nullified are filtered out
    /// cheaply via a single `status()` RPC call.
    ///
    /// Individual game query failures are logged and skipped so that a transient
    /// RPC error on one game does not abort the entire scan. Errored games are
    /// naturally retried on the next tick. After evaluation, the
    /// `base_challenger_games_scanned_total` counter and
    /// `base_challenger_scan_head` gauge are updated.
    pub async fn scan(&self) -> Result<Vec<CandidateGame>> {
        let game_count = self.factory_client.game_count().await?;

        if game_count == 0 {
            debug!("factory has no games");
            return Ok(vec![]);
        }

        let end = game_count - 1;
        let start = game_count.saturating_sub(self.config.lookback_games);

        let games_to_scan = end - start + 1;

        let results: Vec<(u64, Result<Option<CandidateGame>>)> = stream::iter(start..=end)
            .map(|i| async move { (i, self.evaluate_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .collect()
            .await;

        let mut candidates = Vec::new();

        for (i, result) in results {
            match result {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, index = i, "failed to query game, skipping");
                }
            }
        }

        candidates.sort_unstable_by_key(|c| c.index);

        ChallengerMetrics::games_scanned_total().increment(games_to_scan);
        ChallengerMetrics::scan_head().set(end as f64);

        info!(
            games_found = candidates.len(),
            scan_head = end,
            games_scanned = games_to_scan,
            "scan complete"
        );

        Ok(candidates)
    }

    /// Evaluates a single game at the given factory index.
    ///
    /// Returns `Some(CandidateGame)` if the game is `IN_PROGRESS` and
    /// matches one of the four [`GameCategory`] variants. Returns `None`
    /// if the game should be skipped (resolved, fully nullified, or in
    /// an unrecognized state).
    pub async fn evaluate_game(&self, index: u64) -> Result<Option<CandidateGame>> {
        let factory = self.factory_client.game_at_index(index).await?;

        let status = self.verifier_client.status(factory.proxy).await?;
        if status != Self::STATUS_IN_PROGRESS {
            debug!(index = index, status = status, "skipping game not in progress");
            return Ok(None);
        }

        // Fetch classification fields only for in-progress games.
        let (zk_prover, tee_prover, countered_index) = tokio::try_join!(
            self.verifier_client.zk_prover(factory.proxy),
            self.verifier_client.tee_prover(factory.proxy),
            self.verifier_client.countered_index(factory.proxy),
        )?;

        let category = match Self::classify(index, tee_prover, zk_prover, countered_index) {
            Some(c) => c,
            None => return Ok(None),
        };

        // Fetch remaining fields only for actionable games.
        let ((info, starting_block_number, l1_head), intermediate_block_interval) = tokio::try_join!(
            async {
                tokio::try_join!(
                    self.verifier_client.game_info(factory.proxy),
                    self.verifier_client.starting_block_number(factory.proxy),
                    self.verifier_client.l1_head(factory.proxy),
                )
                .map_err(Into::into)
            },
            self.resolve_intermediate_block_interval(factory.game_type),
        )?;

        Ok(Some(CandidateGame {
            index,
            factory,
            info,
            starting_block_number,
            intermediate_block_interval,
            l1_head,
            tee_prover,
            category,
        }))
    }

    /// Classifies a game into a [`GameCategory`] based on its prover state,
    /// or returns `None` if the game should be skipped.
    fn classify(
        index: u64,
        tee_prover: Address,
        zk_prover: Address,
        countered_index: u64,
    ) -> Option<GameCategory> {
        let has_tee = tee_prover != Address::ZERO;
        let has_zk = zk_prover != Address::ZERO;

        match (has_tee, has_zk, countered_index) {
            // Path 1: TEE-proposed, unchallenged.
            (true, false, 0) => Some(GameCategory::InvalidTeeProposal),

            // TEE-only game with a non-zero countered_index — unexpected state.
            (true, false, ci) => {
                debug!(
                    index = index,
                    countered_index = ci,
                    "skipping TEE-only game with unexpected non-zero countered_index"
                );
                None
            }

            // TEE + ZK present but no countered index — second proof was added
            // via `verifyProposalProof`, not via `challenge`. Both proofs may
            // still verify an incorrect root. Nullify the TEE proof first
            // (fast) then the ZK proof on the next scan.
            (true, true, 0) => {
                debug!(index = index, "dual-proof game selected for validation");
                Some(GameCategory::InvalidDualProposal)
            }

            // Path 2: TEE-proposed and challenged by ZK.
            (true, true, ci) => {
                debug_assert!(ci > 0, "ci == 0 should be handled by (true, true, 0) arm");
                Some(GameCategory::FraudulentZkChallenge { challenged_index: ci - 1 })
            }

            // Path 3: ZK-proposed, unchallenged.
            (false, true, 0) => Some(GameCategory::InvalidZkProposal),

            // ZK-only game with a non-zero countered_index — unexpected state.
            (false, true, ci) => {
                debug!(
                    index = index,
                    countered_index = ci,
                    "skipping ZK-only game with unexpected non-zero countered_index"
                );
                None
            }

            // Both provers zeroed — already nullified.
            (false, false, _) => {
                debug!(index = index, "skipping nullified game (both provers zeroed)");
                None
            }
        }
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
