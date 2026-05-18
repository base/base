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
    collections::{BTreeMap, HashMap},
    mem,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient, GameAtIndex,
    GameInfo, GameStatus,
};
use eyre::Result;
use futures::stream::{self, StreamExt};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, warn};

use crate::ChallengerMetrics;

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
/// The scanner uses these variants to decide whether to keep an index in
/// its persistent tracking map.
#[derive(Debug)]
pub enum GameEvaluation {
    /// `IN_PROGRESS` and matches a [`GameCategory`] — the driver should act.
    Actionable(CandidateGame),

    /// `IN_PROGRESS` but currently in an unexpected/unactionable state
    /// (e.g. TEE-only with non-zero `countered_index`). A later on-chain
    /// transition could make it actionable, so it stays tracked.
    InProgressNotActionable,

    /// Resolved (`status != IN_PROGRESS`) or fully nullified (both provers
    /// zero). No future transition can make it actionable.
    Terminal,
}

/// Scans the `DisputeGameFactory` for dispute games that need validation.
///
/// On every tick the scanner locates the current anchor game in the factory
/// index list, then evaluates every later factory index. This avoids an
/// arbitrary lookback cap while still skipping historical games at or before
/// the accepted anchor.
///
/// Indices are removed from the tracking set as soon as their game reaches
/// a terminal state (resolved or fully nullified), bounding memory use to
/// the number of currently-live post-anchor games rather than the total
/// factory size.
pub struct GameScanner {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
    /// Serializes scan ticks so scanner-local caches are updated from one
    /// coherent view of the factory and anchor registry.
    scan_lock: AsyncMutex<()>,
    /// Cache of `(game_type, impl_address) → intermediate_block_interval` to avoid repeated
    /// RPC calls. Keyed on both fields so that a governance `setImplementation` call
    /// (which changes the impl address) automatically causes a cache miss.
    interval_cache: Mutex<HashMap<(u32, Address), u64>>,
    /// Cached `(anchor_game, factory_index)` for the current anchor game.
    anchor_index: Mutex<Option<(Address, u64)>>,
    /// Tracked factory indices keyed to their consecutive scan-failure
    /// count (`0` for healthy entries). An index is present iff the game was
    /// observed `IN_PROGRESS` on a previous tick.
    tracking: Mutex<BTreeMap<u64, u64>>,
}

impl std::fmt::Debug for GameScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameScanner").finish_non_exhaustive()
    }
}

impl GameScanner {
    /// Maximum number of games to evaluate concurrently during a scan.
    pub const SCAN_CONCURRENCY: usize = 32;

    /// Maximum number of factory indices to inspect in one anchor lookup batch.
    pub const ANCHOR_SEARCH_BATCH_SIZE: u64 = 1024;

    /// Consecutive per-index scan failures before logs escalate to `error!`.
    pub const PERSISTENT_SCAN_ERROR_LOG_THRESHOLD: u64 = 3;

    /// Creates a new game scanner.
    pub fn new(
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
    ) -> Self {
        Self {
            factory_client,
            verifier_client,
            anchor_registry_client,
            scan_lock: AsyncMutex::new(()),
            interval_cache: Mutex::new(HashMap::new()),
            anchor_index: Mutex::new(None),
            tracking: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the number of game indices currently retained in the
    /// in-progress tracking map.
    pub fn tracked_indices_len(&self) -> usize {
        self.tracking.lock().expect("tracking lock poisoned").len()
    }

    /// Scans for candidate games that need validation.
    ///
    /// Every call evaluates every factory index after the current anchor game.
    /// If the registry is still at its starting anchor (`anchorGame == 0`), the
    /// scan starts from index 0. If the anchor game cannot be found in the
    /// factory, the scanner falls back to index 0 rather than risking a missed
    /// game.
    ///
    /// Games are filtered out cheaply via a single `status()` RPC call when
    /// they are no longer `IN_PROGRESS`. Individual game query failures are
    /// logged and skipped so that a transient RPC error on one game does not
    /// abort the entire scan; previously-tracked failing indices are retained
    /// so repeated failures can be surfaced with escalating logs. After
    /// evaluation, the
    /// `base_challenger_games_scanned_total` counter,
    /// `base_challenger_scan_tracked_in_progress` gauge, and
    /// `base_challenger_scan_head` gauge are updated.
    pub async fn scan(&self) -> Result<Vec<CandidateGame>> {
        let _scan_guard = self.scan_lock.lock().await;
        let game_count = self.factory_client.game_count().await?;

        if game_count == 0 {
            debug!("factory has no games");
            // A factory with zero games invalidates any tracking we accumulated
            // (e.g. the factory address was reconfigured at runtime).
            self.tracking.lock().expect("tracking lock poisoned").clear();
            ChallengerMetrics::scan_tracked_in_progress().set(0.0);
            return Ok(vec![]);
        }

        let end = game_count - 1;
        let scan_start = self.scan_start_index(game_count).await;

        let previous_tracking =
            mem::take(&mut *self.tracking.lock().expect("tracking lock poisoned"));
        let pruned_tracking = previous_tracking.keys().filter(|&&i| i < scan_start).count();
        if pruned_tracking > 0 {
            info!(
                pruned_tracking = pruned_tracking,
                scan_start = scan_start,
                "pruned tracked indices behind anchor"
            );
        }
        let games_to_scan = game_count.saturating_sub(scan_start);

        let results: Vec<(u64, Result<GameEvaluation>)> = stream::iter(scan_start..game_count)
            .map(|i| async move { (i, self.evaluate_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .collect()
            .await;

        let mut candidates = Vec::new();
        let mut next_tracking: BTreeMap<u64, u64> = BTreeMap::new();

        for (i, result) in results {
            match result {
                Ok(GameEvaluation::Actionable(candidate)) => {
                    next_tracking.insert(i, 0);
                    candidates.push(candidate);
                }
                Ok(GameEvaluation::InProgressNotActionable) => {
                    next_tracking.insert(i, 0);
                }
                Ok(GameEvaluation::Terminal) => {}
                Err(e) => {
                    let consecutive_failures =
                        previous_tracking.get(&i).copied().unwrap_or_default() + 1;
                    if consecutive_failures >= Self::PERSISTENT_SCAN_ERROR_LOG_THRESHOLD {
                        error!(error = %e, index = i, consecutive_failures, "game query failed");
                    } else {
                        warn!(error = %e, index = i, consecutive_failures, "game query failed");
                    }
                    if previous_tracking.contains_key(&i) {
                        // Keep previously-tracked indices so repeated failures are
                        // visible in metrics and logs.
                        next_tracking.insert(i, consecutive_failures);
                    }
                }
            }
        }

        candidates.sort_unstable_by_key(|c| c.index);

        let tracked_len = next_tracking.len();
        *self.tracking.lock().expect("tracking lock poisoned") = next_tracking;

        ChallengerMetrics::games_scanned_total().increment(games_to_scan);
        ChallengerMetrics::scan_tracked_in_progress().set(tracked_len as f64);
        ChallengerMetrics::scan_head().set(end as f64);

        info!(
            games_found = candidates.len(),
            scan_start,
            scan_head = end,
            games_scanned = games_to_scan,
            tracked_in_progress = tracked_len,
            "scan complete"
        );

        Ok(candidates)
    }

    /// Returns the first factory index that should be evaluated this tick.
    ///
    /// The start is one past the current anchor game's factory index. When the
    /// registry still has no anchor game, or if the anchor game cannot be
    /// located in this factory, the scanner starts at 0.
    pub async fn scan_start_index(&self, game_count: u64) -> u64 {
        let cached_anchor = *self.anchor_index.lock().expect("anchor_index lock poisoned");
        let cached_scan_start = cached_anchor
            .map(|(_, index)| index)
            .filter(|&index| index < game_count)
            .map(|index| index.saturating_add(1).min(game_count));

        let anchor = match self.anchor_registry_client.anchor_snapshot().await {
            Ok(anchor) => anchor,
            Err(e) => {
                let scan_start = cached_scan_start.unwrap_or_default();
                warn!(
                    error = %e,
                    scan_start = scan_start,
                    has_cached_anchor = cached_scan_start.is_some(),
                    "failed to read anchor snapshot"
                );
                return scan_start;
            }
        };
        let anchor_game = anchor.anchor_game;

        let (next_cached_anchor, scan_start) = if anchor_game == Address::ZERO {
            (None, 0)
        } else if let Some((cached_game, cached_index)) = cached_anchor
            && cached_game == anchor_game
            && cached_index < game_count
        {
            return cached_index.saturating_add(1).min(game_count);
        } else {
            let search_start = cached_anchor
                .map(|(_, index)| index.saturating_add(1))
                .unwrap_or_default()
                .min(game_count);

            let (mut found, mut lookup_had_errors) =
                self.find_game_index(anchor_game, search_start, game_count).await;

            if found.is_none() && search_start > 0 {
                let (wrapped_found, wrapped_lookup_had_errors) =
                    self.find_game_index(anchor_game, 0, search_start).await;
                found = wrapped_found;
                lookup_had_errors |= wrapped_lookup_had_errors;
            }

            if found.is_none()
                && lookup_had_errors
                && let Some(scan_start) = cached_scan_start
            {
                warn!(
                    anchor_game = %anchor_game,
                    scan_start = scan_start,
                    "anchor game not found after lookup errors, using cached anchor"
                );
                return scan_start;
            }

            found.map_or_else(
                || {
                    warn!(
                        anchor_game = %anchor_game,
                        game_count,
                        "anchor game not found in factory, scanning from genesis"
                    );
                    (None, 0)
                },
                |index| (Some((anchor_game, index)), index.saturating_add(1).min(game_count)),
            )
        };

        *self.anchor_index.lock().expect("anchor_index lock poisoned") = next_cached_anchor;
        scan_start
    }

    /// Finds `target` in the half-open factory index range `[start, end)`,
    /// searching backward in batches and returning the match closest to `end`.
    ///
    /// This is optimized for anchor lookup, where game proxy addresses are
    /// unique and the current anchor usually sits near the tail of the factory.
    pub async fn find_game_index(
        &self,
        target: Address,
        start: u64,
        end: u64,
    ) -> (Option<u64>, bool) {
        if start >= end {
            return (None, false);
        }

        let mut search_end = end;
        let mut had_errors = false;

        while search_end > start {
            let search_start = search_end.saturating_sub(Self::ANCHOR_SEARCH_BATCH_SIZE).max(start);
            let results: Vec<_> = stream::iter(search_start..search_end)
                .map(|i| async move { (i, self.factory_client.game_at_index(i).await) })
                .buffer_unordered(Self::SCAN_CONCURRENCY)
                .collect()
                .await;

            let mut found = None;
            for (index, result) in results {
                let game = match result {
                    Ok(game) => game,
                    Err(e) => {
                        had_errors = true;
                        warn!(
                            error = %e,
                            index = index,
                            "failed to fetch game during anchor search"
                        );
                        continue;
                    }
                };
                if game.proxy == target {
                    found = Some(found.map_or(index, |current: u64| current.max(index)));
                }
            }

            if found.is_some() {
                return (found, had_errors);
            }

            search_end = search_start;
        }

        (None, had_errors)
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
        if status != GameStatus::InProgress {
            debug!(index = index, status = %status, "game has resolved");
            return Ok(GameEvaluation::Terminal);
        }

        let (zk_prover, tee_prover, countered_index) = tokio::try_join!(
            self.verifier_client.zk_prover(factory.proxy),
            self.verifier_client.tee_prover(factory.proxy),
            self.verifier_client.countered_index(factory.proxy),
        )?;

        // Both provers zero means the game has been fully nullified and no
        // future on-chain transition can make it actionable.
        if tee_prover == Address::ZERO && zk_prover == Address::ZERO {
            debug!(index = index, "game fully nullified (both provers zeroed)");
            return Ok(GameEvaluation::Terminal);
        }

        let category = match Self::classify(index, tee_prover, zk_prover, countered_index) {
            Some(c) => c,
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
    /// Callers should filter fully-nullified games before classification so
    /// they can be treated as terminal instead of unexpectedly in-progress.
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

            // Unreachable: `ci > 0` requires `challenge()` (which sets `zkProver`),
            // and clearing `zkProver` runs through `_proofRefutedUpdate(ZK)` which
            // also clears `ci`. Suspect contract bug if observed.
            (true, false, ci) => {
                error!(
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

            // Only reachable after a global `TEE_VERIFIER.nullify()` drops the
            // TEE proof on a game with an active challenge (`_updateProofCount`
            // does not clear `ci` for TEE refutations). Requires a TEE soundness
            // break or key compromise.
            (false, true, ci) => {
                warn!(
                    index = index,
                    countered_index = ci,
                    "skipping ZK-only game with unexpected non-zero countered_index"
                );
                None
            }

            // Caller is responsible for filtering out fully-nullified games.
            (false, false, _) => {
                warn!(index = index, "fully-nullified game reached classifier");
                None
            }
        }
    }

    /// Resolves the intermediate block interval for a game type, using a cache
    /// to avoid repeated RPC calls for the same `(game_type, impl_address)` pair.
    ///
    /// The impl address is always fetched from the factory so that a governance
    /// `setImplementation` call (which changes the address) automatically
    /// invalidates the cached value.
    async fn resolve_intermediate_block_interval(&self, game_type: u32) -> Result<u64> {
        let impl_address = self.factory_client.game_impls(game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no game implementation registered in DisputeGameFactory for game type {game_type}"
            ));
        }

        let cache_key = (game_type, impl_address);

        {
            let cache = self.interval_cache.lock().expect("interval_cache lock poisoned");
            if let Some(&interval) = cache.get(&cache_key) {
                return Ok(interval);
            }
        }

        let interval = self.verifier_client.read_intermediate_block_interval(impl_address).await?;

        debug!(
            game_type = game_type,
            interval = interval,
            impl_address = %impl_address,
            "resolved intermediate block interval"
        );

        let mut cache = self.interval_cache.lock().expect("interval_cache lock poisoned");
        cache.insert(cache_key, interval);

        Ok(interval)
    }
}
