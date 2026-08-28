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
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient, GameAtIndex,
    GameInfo, GameStatus, ProofArtifacts,
};
use eyre::Result;
use futures::stream::{self, StreamExt};
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
    /// Proving artifacts committed by this game's verifier.
    pub proof_artifacts: ProofArtifacts,
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
/// On every tick the scanner locates the current anchor game in the factory
/// index list, then evaluates every later factory index. This avoids an
/// arbitrary lookback cap while still skipping historical games at or before
/// the accepted anchor.
pub struct GameScanner {
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    anchor_registry_client: Arc<dyn AnchorStateRegistryClient>,
    /// Cache of `impl_address → intermediate_block_interval` to avoid repeated RPC calls.
    /// A governance `setImplementation` call changes the address and automatically causes a
    /// cache miss.
    interval_cache: Mutex<HashMap<Address, u64>>,
    /// Cached `(anchor_game, factory_index)` for the current anchor game.
    anchor_index: Option<(Address, u64)>,
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
            interval_cache: Mutex::new(HashMap::new()),
            anchor_index: None,
        }
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
    /// abort the entire scan. A later scan retries the whole post-anchor range.
    /// After evaluation, the `base_challenger_games_scanned_total` counter and
    /// `base_challenger_scan_head` gauge are updated.
    pub async fn scan(&mut self) -> Result<Vec<CandidateGame>> {
        let game_count = self.factory_client.game_count().await?;

        if game_count == 0 {
            debug!("factory has no games");
            return Ok(vec![]);
        }

        let end = game_count - 1;
        let scan_start = self.scan_start_index(game_count).await;

        let games_to_scan = game_count.saturating_sub(scan_start);

        let scanner = &*self;
        let results: Vec<(u64, Result<Option<CandidateGame>>)> =
            stream::iter(scan_start..game_count)
                .map(|i| async move { (i, scanner.evaluate_game(i).await) })
                .buffer_unordered(Self::SCAN_CONCURRENCY)
                .collect()
                .await;

        let mut candidates = Vec::new();

        for (i, result) in results {
            match result {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => warn!(error = %e, index = i, "game query failed"),
            }
        }

        candidates.sort_unstable_by_key(|c| c.index);

        ChallengerMetrics::games_scanned_total().increment(games_to_scan);
        ChallengerMetrics::scan_head().set(end as f64);

        info!(
            games_found = candidates.len(),
            scan_start,
            scan_head = end,
            games_scanned = games_to_scan,
            "scan complete"
        );

        Ok(candidates)
    }

    /// Returns the first factory index that should be evaluated this tick.
    ///
    /// The start is one past the current anchor game's factory index. When the
    /// registry still has no anchor game, or if the anchor game cannot be
    /// located in this factory, the scanner starts at 0.
    pub async fn scan_start_index(&mut self, game_count: u64) -> u64 {
        let cached_anchor = self.anchor_index;
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

        self.anchor_index = next_cached_anchor;
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
    /// Returns a candidate when the game is actionable, otherwise `None`.
    pub async fn evaluate_game(&self, index: u64) -> Result<Option<CandidateGame>> {
        let factory = self.factory_client.game_at_index(index).await?;

        let status = self.verifier_client.status(factory.proxy).await?;
        if status != GameStatus::InProgress {
            debug!(index = index, status = %status, "game has resolved");
            return Ok(None);
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
            return Ok(None);
        }

        let category = match Self::classify(index, tee_prover, zk_prover, countered_index) {
            Some(c) => c,
            None => return Ok(None),
        };

        // Fetch remaining fields only for actionable games.
        let ((info, starting_block_number, l1_head, proof_artifacts), intermediate_block_interval) =
            tokio::try_join!(
                async {
                    tokio::try_join!(
                        self.verifier_client.game_info(factory.proxy),
                        self.verifier_client.starting_block_number(factory.proxy),
                        self.verifier_client.l1_head(factory.proxy),
                        self.verifier_client.proof_artifacts(factory.proxy),
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
            proof_artifacts,
            tee_prover,
            category,
        }))
    }

    /// Classifies a game into a [`GameCategory`] based on its prover state,
    /// or returns `None` if the game is in an unexpected state.
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
    /// to avoid repeated RPC calls for the same implementation address.
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

        {
            let cache = self.interval_cache.lock().expect("interval_cache lock poisoned");
            if let Some(&interval) = cache.get(&impl_address) {
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
        cache.insert(impl_address, interval);

        Ok(interval)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use alloy_primitives::{Address, B256, Bytes, U256};
    use async_trait::async_trait;
    use base_proof_contracts::{
        AggregateVerifierClient, AnchorStateRegistryClient, ContractError,
        DisputeGameFactoryClient, GameAtIndex, GameStatus,
    };

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, addr, factory_game,
        mock_anchor_registry, mock_state, mock_state_with_tee,
    };

    /// Mock factory that records queried indices and can return errors for specific indices.
    #[derive(Debug)]
    struct RecordingDisputeGameFactory {
        /// The inner factory providing normal game data.
        inner: MockDisputeGameFactory,
        /// Indices that should return an error when queried.
        error_indices: Vec<u64>,
        /// Factory indices queried through `game_at_index`.
        queried_indices: Mutex<Vec<u64>>,
    }

    impl RecordingDisputeGameFactory {
        /// Creates a new recording factory.
        fn new(games: Vec<GameAtIndex>, error_indices: Vec<u64>) -> Self {
            Self {
                inner: MockDisputeGameFactory::new(games),
                error_indices,
                queried_indices: Mutex::new(Vec::new()),
            }
        }

        /// Returns all indices queried so far.
        fn queried_indices(&self) -> Vec<u64> {
            self.queried_indices.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DisputeGameFactoryClient for RecordingDisputeGameFactory {
        async fn game_count(&self) -> Result<u64, ContractError> {
            self.inner.game_count().await
        }

        async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
            self.queried_indices.lock().unwrap().push(index);
            if self.error_indices.contains(&index) {
                return Err(ContractError::Validation(format!("simulated error at index {index}")));
            }
            self.inner.game_at_index(index).await
        }

        async fn init_bonds(&self, game_type: u32) -> Result<U256, ContractError> {
            self.inner.init_bonds(game_type).await
        }

        async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError> {
            self.inner.game_impls(game_type).await
        }

        async fn games(
            &self,
            game_type: u32,
            root_claim: B256,
            extra_data: Bytes,
        ) -> Result<Address, ContractError> {
            self.inner.games(game_type, root_claim, extra_data).await
        }
    }

    /// Happy path: mixed games, only `IN_PROGRESS` / non-nullified returned.
    #[tokio::test]
    async fn test_scan_happy_path() {
        // Game 0: type 1, IN_PROGRESS, TEE only -> candidate (InvalidTeeProposal)
        // Game 1: type 99, IN_PROGRESS, TEE only -> candidate (all types scanned)
        // Game 2: type 1, status=1 (not in progress) -> skipped
        // Game 3: type 1, IN_PROGRESS, TEE + ZK (dual proof) -> candidate (InvalidDualProposal)
        // Game 4: type 1, IN_PROGRESS, TEE only -> candidate (InvalidTeeProposal)
        let factory = Arc::new(MockDisputeGameFactory::new(vec![
            factory_game(0, 1),
            factory_game(1, 99),
            factory_game(2, 1),
            factory_game(3, 1),
            factory_game(4, 1),
        ]));

        let challenger_addr = Address::repeat_byte(0xCC);
        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(GameStatus::InProgress, Address::ZERO, 100));
        verifier_games.insert(addr(1), mock_state(GameStatus::InProgress, Address::ZERO, 150));
        verifier_games.insert(addr(2), mock_state(GameStatus::ChallengerWins, Address::ZERO, 200));
        verifier_games.insert(addr(3), mock_state(GameStatus::InProgress, challenger_addr, 300));
        verifier_games.insert(addr(4), mock_state(GameStatus::InProgress, Address::ZERO, 400));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(
            factory,
            Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
            mock_anchor_registry(Address::ZERO),
        );

        let candidates = scanner.scan().await.unwrap();

        // start = max(0, 5-1000) = 0, so games 0..=4 scanned
        // Game 0: TEE only -> candidate. Game 1: TEE only -> candidate.
        // Game 2: status != 0 -> skipped.
        // Game 3: dual proof (TEE+ZK, no challenge) -> candidate.
        // Game 4: TEE only -> candidate.
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].factory.game_type, 1);
        assert_eq!(candidates[0].info.l2_block_number, 100);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(candidates[1].factory.game_type, 99);
        assert_eq!(candidates[1].info.l2_block_number, 150);
        assert_eq!(candidates[2].index, 3);
        assert_eq!(candidates[2].category, GameCategory::InvalidDualProposal);
        assert_eq!(candidates[3].index, 4);
        assert_eq!(candidates[3].factory.game_type, 1);
        assert_eq!(candidates[3].info.l2_block_number, 400);
        assert_eq!(
            verifier.intermediate_block_interval_reads.lock().unwrap().as_slice(),
            &[Address::repeat_byte(0x11)],
        );
    }

    /// Dual-proof games (TEE + ZK, no challenge) are now candidates.
    #[tokio::test]
    async fn test_scan_dual_proof_games_are_candidates() {
        let zk_addr = Address::repeat_byte(0xAA);

        let factory = Arc::new(MockDisputeGameFactory::new(vec![
            factory_game(0, 1),
            factory_game(1, 1),
            factory_game(2, 1),
        ]));

        let mut verifier_games = HashMap::new();
        // Game 0: TEE + ZK (dual proof, no challenge) -> candidate (InvalidDualProposal)
        verifier_games.insert(addr(0), mock_state(GameStatus::InProgress, zk_addr, 100));
        // Game 1: TEE only -> candidate (InvalidTeeProposal)
        verifier_games.insert(addr(1), mock_state(GameStatus::InProgress, Address::ZERO, 200));
        // Game 2: TEE + ZK (dual proof, no challenge) -> candidate (InvalidDualProposal)
        verifier_games.insert(addr(2), mock_state(GameStatus::InProgress, zk_addr, 300));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[0].category, GameCategory::InvalidDualProposal);
        assert_eq!(candidates[1].index, 1);
        assert_eq!(candidates[1].category, GameCategory::InvalidTeeProposal);
        assert_eq!(candidates[2].index, 2);
        assert_eq!(candidates[2].category, GameCategory::InvalidDualProposal);
    }

    /// Empty factory returns empty vec without error.
    #[tokio::test]
    async fn test_scan_empty_factory() {
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert!(candidates.is_empty());
    }

    /// Anchor lower bound: only games after the current anchor game are scanned.
    #[tokio::test]
    async fn test_scan_starts_after_anchor_game() {
        // Factory with 100 games and anchor at index 96 -> scan indices 97, 98, 99.
        let mut games = Vec::new();
        let mut verifier_games = HashMap::new();

        for i in 0..100u64 {
            games.push(factory_game(i, 1));
            verifier_games
                .insert(addr(i), mock_state(GameStatus::InProgress, Address::ZERO, i * 10));
        }

        let factory = Arc::new(MockDisputeGameFactory::new(games));
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(addr(96)));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].index, 97);
        assert_eq!(candidates[1].index, 98);
        assert_eq!(candidates[2].index, 99);
    }

    /// Cold anchor lookup searches the latest batch first and skips individual
    /// lookup errors without aborting the whole search.
    #[tokio::test]
    async fn test_find_game_index_searches_tail_batch_and_skips_errors() {
        let game_count = GameScanner::ANCHOR_SEARCH_BATCH_SIZE + 10;
        let target_index = game_count - 1;
        let error_index = target_index - 1;

        let games = (0..game_count).map(|i| factory_game(i, 1)).collect();
        let factory = Arc::new(RecordingDisputeGameFactory::new(games, vec![error_index]));
        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));
        let scanner = GameScanner::new(
            Arc::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
            verifier,
            mock_anchor_registry(Address::ZERO),
        );

        let (found, had_errors) = scanner.find_game_index(addr(target_index), 0, game_count).await;

        assert_eq!(found, Some(target_index));
        assert!(had_errors, "lookup should report skipped per-index errors");

        let queried = factory.queried_indices();
        let first_tail_index = game_count - GameScanner::ANCHOR_SEARCH_BATCH_SIZE;
        assert_eq!(queried.len() as u64, GameScanner::ANCHOR_SEARCH_BATCH_SIZE);
        assert!(
            queried.iter().all(|&i| i >= first_tail_index && i < game_count),
            "cold lookup should only query the latest anchor search batch"
        );
    }

    /// If a factory ever reused a proxy address, anchor lookup should return
    /// the matching index nearest the end of the searched range.
    #[tokio::test]
    async fn test_find_game_index_returns_match_closest_to_end() {
        let target = addr(99);
        let mut games =
            vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1), factory_game(3, 1)];
        games[1].proxy = target;
        games[3].proxy = target;

        let factory = Arc::new(RecordingDisputeGameFactory::new(games, vec![]));
        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));
        let scanner = GameScanner::new(
            Arc::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
            verifier,
            mock_anchor_registry(Address::ZERO),
        );

        let (found, had_errors) = scanner.find_game_index(target, 0, 4).await;

        assert_eq!(found, Some(3));
        assert!(!had_errors);
    }

    /// If reading the anchor snapshot fails after a cache has been populated,
    /// the scanner keeps using the cached anchor instead of scanning genesis.
    #[tokio::test]
    async fn test_scan_uses_cached_anchor_when_anchor_snapshot_fails() {
        let mut games = Vec::new();
        let mut verifier_games = HashMap::new();

        for i in 0..5u64 {
            games.push(factory_game(i, 1));
            verifier_games
                .insert(addr(i), mock_state(GameStatus::InProgress, Address::ZERO, i * 10));
        }

        let factory = Arc::new(MockDisputeGameFactory::new(games));
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let anchor_registry = Arc::new(MockAnchorStateRegistry::new(addr(2)));
        let mut scanner = GameScanner::new(
            factory,
            verifier,
            Arc::clone(&anchor_registry) as Arc<dyn AnchorStateRegistryClient>,
        );

        let initial = scanner.scan().await.unwrap();
        assert_eq!(initial.iter().map(|c| c.index).collect::<Vec<_>>(), vec![3, 4]);

        anchor_registry.set_fail_snapshot(true);
        let cached = scanner.scan().await.unwrap();

        assert_eq!(cached.iter().map(|c| c.index).collect::<Vec<_>>(), vec![3, 4]);
    }

    /// Error resilience: a per-game error is logged and skipped, other games still returned.
    /// Errored games are naturally retried on the next scan since the full post-anchor
    /// range is always evaluated.
    #[tokio::test]
    async fn test_scan_skips_errored_games() {
        // 3 games: index 1 will error, indices 0 and 2 are valid candidates
        let factory = Arc::new(RecordingDisputeGameFactory::new(
            vec![factory_game(0, 1), factory_game(1, 1), factory_game(2, 1)],
            vec![1],
        ));

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(GameStatus::InProgress, Address::ZERO, 100));
        // index 1 won't be queried on the verifier because the factory errors first
        verifier_games.insert(addr(2), mock_state(GameStatus::InProgress, Address::ZERO, 300));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        // Index 0 -> candidate. Index 1 errors -> skipped. Index 2 -> candidate.
        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 2);
    }

    /// Games with a non-zero TEE prover but zero ZK prover are still candidates.
    ///
    /// A non-zero `teeProver` with `zkProver == ZERO` is the normal initial
    /// state for an unchallenged game. The scanner should return these as
    /// candidates.
    #[tokio::test]
    async fn test_scan_tee_prover_nonzero_still_candidate() {
        let tee_addr = Address::repeat_byte(0xEE);

        let factory =
            Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1), factory_game(1, 1)]));

        let mut verifier_games = HashMap::new();
        // Game 0: IN_PROGRESS, no ZK prover, has a TEE prover -> candidate
        verifier_games.insert(
            addr(0),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, 100),
        );
        // Game 1: IN_PROGRESS, no ZK prover, has default TEE prover -> candidate
        verifier_games.insert(addr(1), mock_state(GameStatus::InProgress, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].index, 0);
        assert_eq!(candidates[1].index, 1);
    }

    /// Error at the first index (0) skips that game, rest still returned.
    #[tokio::test]
    async fn test_scan_error_at_first_index() {
        let factory = Arc::new(RecordingDisputeGameFactory::new(
            vec![factory_game(0, 1), factory_game(1, 1)],
            vec![0],
        ));

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(1), mock_state(GameStatus::InProgress, Address::ZERO, 200));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
    }

    /// A challenged game (TEE + ZK provers non-zero, `countered_index` > 0) is
    /// returned as a [`GameCategory::FraudulentZkChallenge`] candidate.
    #[tokio::test]
    async fn test_scan_challenged_game_returns_fraudulent_zk_challenge() {
        let tee_addr = Address::repeat_byte(0xEE);
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]));

        let mut verifier_games = HashMap::new();
        let mut state = mock_state_with_tee(GameStatus::InProgress, zk_addr, tee_addr, 100);
        state.countered_index = 3; // 1-based: challenged at 0-based index 2
        verifier_games.insert(addr(0), state);

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].category,
            GameCategory::FraudulentZkChallenge { challenged_index: 2 }
        );
    }

    /// A ZK-proposed game (`tee_prover` == 0, `zk_prover` != 0, unchallenged) is
    /// returned as a [`GameCategory::InvalidZkProposal`] candidate.
    #[tokio::test]
    async fn test_scan_zk_proposal_returns_invalid_zk_proposal() {
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]));

        let mut verifier_games = HashMap::new();
        // tee_prover == ZERO, zk_prover != ZERO, countered_index == 0
        verifier_games.insert(
            addr(0),
            mock_state_with_tee(GameStatus::InProgress, zk_addr, Address::ZERO, 100),
        );

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].category, GameCategory::InvalidZkProposal);
    }

    /// A TEE-proposed unchallenged game is returned as
    /// [`GameCategory::InvalidTeeProposal`].
    #[tokio::test]
    async fn test_scan_tee_proposal_returns_invalid_tee_proposal() {
        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]));

        let mut verifier_games = HashMap::new();
        verifier_games.insert(addr(0), mock_state(GameStatus::InProgress, Address::ZERO, 100));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].category, GameCategory::InvalidTeeProposal);
    }

    /// A game with both proofs verified (TEE + ZK, no challenge) is a
    /// candidate for validation. Both proofs may verify a wrong root.
    #[tokio::test]
    async fn test_scan_both_proofs_verified_is_candidate() {
        let tee_addr = Address::repeat_byte(0xEE);
        let zk_addr = Address::repeat_byte(0xCC);

        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]));

        let mut verifier_games = HashMap::new();
        // Both provers non-zero, countered_index == 0 (added via verifyProposalProof)
        verifier_games
            .insert(addr(0), mock_state_with_tee(GameStatus::InProgress, zk_addr, tee_addr, 100));

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1, "dual-proof game should be a candidate");
        assert_eq!(candidates[0].category, GameCategory::InvalidDualProposal);
    }

    /// Games with both `teeProver` and `zkProver` at `Address::ZERO` are
    /// filtered out. Every game is initialized with at least one prover, so
    /// both being zero indicates a prior nullification.
    #[tokio::test]
    async fn test_scan_filters_nullified_games() {
        let tee_addr = Address::repeat_byte(0xEE);

        let factory = Arc::new(MockDisputeGameFactory::new(vec![
            factory_game(0, 1),
            factory_game(1, 1),
            factory_game(2, 1),
        ]));

        let mut verifier_games = HashMap::new();
        // Game 0: both provers zeroed (nullified) → filtered out
        verifier_games.insert(
            addr(0),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, Address::ZERO, 100),
        );
        // Game 1: TEE prover active, ZK prover zero → candidate
        verifier_games.insert(
            addr(1),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, 200),
        );
        // Game 2: both provers zeroed (nullified) → filtered out
        verifier_games.insert(
            addr(2),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, Address::ZERO, 300),
        );

        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(factory, verifier, mock_anchor_registry(Address::ZERO));

        let candidates = scanner.scan().await.unwrap();

        assert_eq!(candidates.len(), 1, "only the non-nullified game should be a candidate");
        assert_eq!(candidates[0].index, 1);
    }

    /// A game remains covered after new games are appended because the scanner
    /// evaluates the full post-anchor range rather than a rolling tail.
    #[tokio::test]
    async fn test_scan_revisits_old_post_anchor_in_progress_games() {
        let tee_addr = Address::repeat_byte(0xEE);
        let zk_addr = Address::repeat_byte(0xCC);

        // Initial state: 3 games after the starting anchor.
        let factory = Arc::new(MockDisputeGameFactory::new(vec![
            factory_game(0, 1),
            factory_game(1, 1),
            factory_game(2, 1),
        ]));

        let mut verifier_games = HashMap::new();
        verifier_games.insert(
            addr(0),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, 100),
        );
        verifier_games.insert(
            addr(1),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, 200),
        );
        verifier_games.insert(
            addr(2),
            mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, 300),
        );
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let mut scanner = GameScanner::new(
            Arc::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
            Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
            mock_anchor_registry(Address::ZERO),
        );

        // First tick: all three games are post-anchor and discovered.
        let initial = scanner.scan().await.unwrap();
        assert_eq!(initial.len(), 3, "all three initial games are actionable");
        assert!(initial.iter().any(|c| c.index == 0));

        // Simulate a late ZK challenge against game 0 while it remains
        // IN_PROGRESS, then push three new games into the factory.
        let mut challenged_state =
            mock_state_with_tee(GameStatus::InProgress, zk_addr, tee_addr, 100);
        challenged_state.countered_index = 2; // 1-based → challenged_index = 1
        verifier.update_game(addr(0), challenged_state);

        for i in 3..6u64 {
            factory.push(factory_game(i, 1));
            verifier.update_game(
                addr(i),
                mock_state_with_tee(GameStatus::InProgress, Address::ZERO, tee_addr, i * 100),
            );
        }

        // Second tick on the same scanner: game 0 is still post-anchor and must be returned.
        let late = scanner.scan().await.unwrap();
        let game_zero = late
            .iter()
            .find(|c| c.index == 0)
            .expect("old post-anchor in-progress game must still be returned by scan()");
        assert_eq!(
            game_zero.category,
            GameCategory::FraudulentZkChallenge { challenged_index: 1 },
            "old game should now classify under its late state transition"
        );
    }
}
