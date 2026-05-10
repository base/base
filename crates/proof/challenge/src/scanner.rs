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
    collections::{BTreeSet, HashMap},
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
    /// will be re-classified as [`GameCategory::InvalidZkProposal`] on the next scan.
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

/// Outcome of a single [`GameScanner::evaluate_game`] call.
///
/// Distinguishes actionable games from games that are still live but not
/// currently actionable, and from games that have reached a terminal state.
/// The scanner uses these variants to decide whether to keep an index in
/// its persistent in-progress tracking set.
#[derive(Debug)]
pub enum GameEvaluation {
    /// The game is `IN_PROGRESS` and matches one of the [`GameCategory`]
    /// variants. The driver should process the contained candidate.
    Actionable(CandidateGame),

    /// The game is `IN_PROGRESS` but is not currently actionable (e.g. an
    /// unexpected prover/`countered_index` combination). Keep the index in
    /// the tracking set because a later on-chain transition could make it
    /// actionable again.
    InProgressNotActionable,

    /// The game has reached a terminal state and never needs to be
    /// re-evaluated. This covers both resolved games (`status != IN_PROGRESS`)
    /// and games that have been fully nullified (`teeProver` and `zkProver`
    /// both zero). Drop the index from tracking.
    Terminal,
}

/// Scans the `DisputeGameFactory` for dispute games that need validation.
///
/// On every tick the scanner evaluates the union of two index sets:
///
/// * The recent **lookback tail** `(game_count - lookback_games) ..=
///   (game_count - 1)`, which acts as a startup catch-up window and as the
///   discovery channel for newly-created games.
/// * A persistent **tracking set** of indices for games that were observed
///   to be `IN_PROGRESS` on a previous tick. This guarantees that older
///   games which age out of the lookback tail continue to be re-evaluated
///   so that late on-chain transitions (e.g. a fraudulent ZK challenge
///   filed against a legacy TEE-only game) are still detected.
///
/// Indices are removed from the tracking set as soon as their game reaches
/// a terminal state (resolved or fully nullified), bounding memory use to
/// the number of currently-live games rather than the total factory size.
pub struct GameScanner {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    config: ScannerConfig,
    /// Cache of `game_type → intermediate_block_interval` to avoid repeated RPC calls.
    interval_cache: Mutex<HashMap<u32, u64>>,
    /// Set of factory indices for games observed as `IN_PROGRESS` on a
    /// previous tick. Persists discovery so that games which age out of
    /// the lookback tail are still re-evaluated until they resolve.
    tracked_indices: Mutex<BTreeSet<u64>>,
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
        Self {
            factory_client,
            verifier_client,
            config,
            interval_cache: Mutex::new(HashMap::new()),
            tracked_indices: Mutex::new(BTreeSet::new()),
        }
    }

    /// Returns the number of game indices currently retained in the
    /// in-progress tracking set.
    pub fn tracked_indices_len(&self) -> usize {
        self.tracked_indices.lock().expect("tracked_indices lock poisoned").len()
    }

    /// Scans for candidate games that need validation.
    ///
    /// Every call evaluates the union of the recent factory tail (the
    /// lookback window) and the persistent tracking set of previously-observed
    /// `IN_PROGRESS` games. This guarantees both that newly-created games are
    /// discovered and that older live games which have aged out of the tail
    /// continue to be re-evaluated until they reach a terminal state.
    ///
    /// Games are filtered out cheaply via a single `status()` RPC call when
    /// they are no longer `IN_PROGRESS`. Individual game query failures are
    /// logged and skipped so that a transient RPC error on one game does not
    /// abort the entire scan; failing indices are conservatively retained in
    /// the tracking set so they are retried on subsequent ticks. After
    /// evaluation, the `base_challenger_games_scanned_total` counter and
    /// `base_challenger_scan_head` gauge are updated.
    pub async fn scan(&self) -> Result<Vec<CandidateGame>> {
        let game_count = self.factory_client.game_count().await?;

        if game_count == 0 {
            debug!("factory has no games");
            // No games on chain means any tracking we may have accumulated
            // is stale (e.g. the factory address was changed at runtime).
            self.tracked_indices.lock().expect("tracked_indices lock poisoned").clear();
            return Ok(vec![]);
        }

        let end = game_count - 1;
        let tail_start = game_count.saturating_sub(self.config.lookback_games);

        // Build the set of indices to scan: the recent factory tail unioned
        // with previously-discovered live games. The tail acts as the
        // discovery channel for newly-created games and as a startup
        // catch-up window; the tracking set carries forward older live
        // games that the tail no longer reaches.
        let mut indices: BTreeSet<u64> = (tail_start..=end).collect();
        {
            let tracked = self.tracked_indices.lock().expect("tracked_indices lock poisoned");
            for &i in tracked.iter() {
                if i <= end {
                    indices.insert(i);
                }
            }
        }
        let games_to_scan = indices.len() as u64;

        let results: Vec<(u64, Result<GameEvaluation>)> = stream::iter(indices.into_iter())
            .map(|i| async move { (i, self.evaluate_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .collect()
            .await;

        let mut candidates = Vec::new();
        let mut next_tracked: BTreeSet<u64> = BTreeSet::new();

        for (i, result) in results {
            match result {
                Ok(GameEvaluation::Actionable(candidate)) => {
                    next_tracked.insert(i);
                    candidates.push(candidate);
                }
                Ok(GameEvaluation::InProgressNotActionable) => {
                    next_tracked.insert(i);
                }
                Ok(GameEvaluation::Terminal) => {
                    // Game has resolved or been fully nullified; never needs
                    // to be revisited.
                }
                Err(e) => {
                    warn!(error = %e, index = i, "failed to query game, skipping");
                    // Conservatively keep the index in the tracking set so it
                    // is retried on the next tick rather than silently lost.
                    next_tracked.insert(i);
                }
            }
        }

        candidates.sort_unstable_by_key(|c| c.index);

        let tracked_len = next_tracked.len();
        *self.tracked_indices.lock().expect("tracked_indices lock poisoned") = next_tracked;

        ChallengerMetrics::games_scanned_total().increment(games_to_scan);
        ChallengerMetrics::scan_head().set(end as f64);

        info!(
            games_found = candidates.len(),
            scan_head = end,
            games_scanned = games_to_scan,
            tracked_in_progress = tracked_len,
            "scan complete"
        );

        Ok(candidates)
    }

    /// Evaluates a single game at the given factory index.
    ///
    /// Returns a [`GameEvaluation`] describing whether the game is
    /// actionable, still live but not currently actionable, or terminal.
    /// The scanner uses these variants to decide whether to keep the index
    /// in its persistent tracking set.
    pub async fn evaluate_game(&self, index: u64) -> Result<GameEvaluation> {
        let factory = self.factory_client.game_at_index(index).await?;

        let status = self.verifier_client.status(factory.proxy).await?;
        if status != Self::STATUS_IN_PROGRESS {
            debug!(index = index, status = status, "game has resolved");
            return Ok(GameEvaluation::Terminal);
        }

        // Fetch classification fields only for in-progress games.
        let (zk_prover, tee_prover, countered_index) = tokio::try_join!(
            self.verifier_client.zk_prover(factory.proxy),
            self.verifier_client.tee_prover(factory.proxy),
            self.verifier_client.countered_index(factory.proxy),
        )?;

        // A game with both provers zero is fully nullified and cannot
        // transition back to an actionable state — drop tracking even though
        // its status is still IN_PROGRESS.
        if tee_prover == Address::ZERO && zk_prover == Address::ZERO {
            debug!(index = index, "game fully nullified (both provers zeroed)");
            return Ok(GameEvaluation::Terminal);
        }

        let category = match Self::classify(index, tee_prover, zk_prover, countered_index) {
            Some(c) => c,
            // The game is in_progress with at least one active prover but in
            // an unexpected combination (e.g. TEE-only with a non-zero
            // countered_index). Keep tracking it because a later state
            // transition could make it actionable.
            None => return Ok(GameEvaluation::InProgressNotActionable),
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

        Ok(GameEvaluation::Actionable(CandidateGame {
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
    /// or returns `None` if the game is in an unexpected state and should
    /// be left as `InProgressNotActionable`.
    ///
    /// Callers must ensure that at least one of `tee_prover` or `zk_prover`
    /// is non-zero; the fully-nullified case is handled by the caller.
    fn classify(
        index: u64,
        tee_prover: Address,
        zk_prover: Address,
        countered_index: u64,
    ) -> Option<GameCategory> {
        let has_tee = tee_prover != Address::ZERO;
        let has_zk = zk_prover != Address::ZERO;
        debug_assert!(has_tee || has_zk, "classify must not be called for fully-nullified games");

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

            // Caller is responsible for filtering out fully-nullified games.
            (false, false, _) => unreachable!(
                "fully-nullified games must be filtered before classify (index = {index})"
            ),
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
