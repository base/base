//! Game scanning.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, ContractError, DisputeGameFactoryClient,
    GameStatus,
};
use futures::{StreamExt, stream};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

/// Snapshot of a dispute game. All fields except `situation` are
/// CWIA-immutable; workers must re-classify against fresh state before
/// acting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameInfo {
    /// Game proxy address.
    pub address: Address,
    /// Index in the `DisputeGameFactory`.
    pub factory_index: u64,
    /// Final output root claimed by the game.
    pub root_claim: B256,
    /// L1 block hash captured at game creation.
    pub l1_head: B256,
    /// L2 block number proposed by the game.
    pub l2_block_number: u64,
    /// L2 block at the start of the game's range.
    pub starting_l2_block: u64,
    /// Intermediate output roots committed at game creation.
    pub intermediate_roots: Box<[B256]>,
    /// Block interval between intermediate root checkpoints.
    pub intermediate_block_interval: u64,
    /// Classification at scan time. Re-verify before acting.
    pub situation: GameSituation,
}

/// On-chain `(teeProver, zkProver, countered)` classification. Only
/// reachable tuples are represented; unreachable ones come back as
/// [`ClassifyError::Unreachable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameSituation {
    /// `(non-zero, 0, 0)`
    TeeOnly,
    /// `(0, non-zero, 0)`
    ZkOnly,
    /// `(non-zero, non-zero, 0)`
    BothProven,
    /// `(non-zero, non-zero, > 0)`
    UnderChallenge {
        /// 0-based index of the challenged intermediate root.
        challenged_index: u64,
    },
    /// `(0, non-zero, > 0)`
    TeeNullifiedDuringChallenge,
    /// `(0, 0, 0)`
    Terminal,
}

impl std::fmt::Display for GameSituation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::TeeOnly => "TeeOnly",
            Self::ZkOnly => "ZkOnly",
            Self::BothProven => "BothProven",
            Self::UnderChallenge { .. } => "UnderChallenge",
            Self::TeeNullifiedDuringChallenge => "TeeNullifiedDuringChallenge",
            Self::Terminal => "Terminal",
        };
        f.write_str(s)
    }
}

impl GameSituation {
    /// Classifies an on-chain `(teeProver, zkProver, countered)` triple.
    /// `countered` is the raw 1-based `counteredByIntermediateRootIndexPlusOne`.
    pub fn classify(
        tee_prover: Address,
        zk_prover: Address,
        countered: u64,
    ) -> Result<Self, ClassifyError> {
        let has_tee = !tee_prover.is_zero();
        let has_zk = !zk_prover.is_zero();
        match (has_tee, has_zk, countered) {
            (true, false, 0) => Ok(Self::TeeOnly),
            (false, true, 0) => Ok(Self::ZkOnly),
            (true, true, 0) => Ok(Self::BothProven),
            (true, true, c) if c > 0 => Ok(Self::UnderChallenge { challenged_index: c - 1 }),
            (false, true, c) if c > 0 => Ok(Self::TeeNullifiedDuringChallenge),
            (false, false, 0) => Ok(Self::Terminal),
            _ => Err(ClassifyError::Unreachable { tee_prover, zk_prover, countered }),
        }
    }
}

/// Critical signal: a tuple the contract should never produce. Indicates
/// a contract upgrade we missed, RPC tampering, or a reading bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClassifyError {
    /// Input does not match any reachable on-chain state.
    #[error(
        "unreachable on-chain state: tee_prover={tee_prover}, zk_prover={zk_prover}, countered={countered}"
    )]
    Unreachable {
        /// Raw `teeProver()`.
        tee_prover: Address,
        /// Raw `zkProver()`.
        zk_prover: Address,
        /// Raw `counteredByIntermediateRootIndexPlusOne`.
        countered: u64,
    },
}

/// Failure mode for the per-game read inside a scan tick.
#[derive(Debug, Error)]
enum ReadGameError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Classification(#[from] ClassifyError),
}

/// Periodic scanner that locates actionable dispute games on the
/// `DisputeGameFactory` and streams them to a downstream consumer.
pub struct GameDiscovery {
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<dyn AggregateVerifierClient>,
    anchor_registry: Arc<dyn AnchorStateRegistryClient>,
    /// `GameType` of every game this challenger acts on. Used to filter
    /// `findLatestGames` during the anchor lookup.
    game_type: u32,
    /// Cached `INTERMEDIATE_BLOCK_INTERVAL` per `(game_type, impl_address)`.
    /// The value is immutable per impl, and keying on `impl_address` makes
    /// a governance `setImplementation` invalidate the entry automatically.
    interval_cache: Mutex<HashMap<(u32, Address), u64>>,
    /// Cached `(anchor_game, factory_index)` from the most recent anchor
    /// resolution. Invalidated when `anchorGame()` changes.
    anchor_index_cache: Mutex<Option<(Address, u64)>>,
}

impl std::fmt::Debug for GameDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameDiscovery").finish_non_exhaustive()
    }
}

impl GameDiscovery {
    /// Maximum number of concurrent RPC reads per scan tick.
    const SCAN_CONCURRENCY: usize = 32;

    /// Number of factory entries inspected per `findLatestGames` call.
    const ANCHOR_LOOKUP_BATCH: u64 = 1024;

    /// Builds a discovery against the given clients. `game_type` is the
    /// dispute-game type this challenger acts on.
    pub fn new(
        factory: Arc<dyn DisputeGameFactoryClient>,
        verifier: Arc<dyn AggregateVerifierClient>,
        anchor_registry: Arc<dyn AnchorStateRegistryClient>,
        game_type: u32,
    ) -> Self {
        Self {
            factory,
            verifier,
            anchor_registry,
            game_type,
            interval_cache: Mutex::new(HashMap::new()),
            anchor_index_cache: Mutex::new(None),
        }
    }

    /// Periodic scan loop. Sleeps `poll_interval` between ticks, sends each
    /// actionable game on `tx`, and exits when `cancel` fires or the
    /// receiver is dropped. Per-tick errors are logged and the loop
    /// continues.
    pub async fn run(
        self,
        tx: mpsc::Sender<GameInfo>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(poll_interval) => {}
            }

            match self.scan().await {
                Ok(games) => {
                    for game in games {
                        if tx.send(game).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => warn!(error = %e, "scan tick failed; will retry next interval"),
            }
        }
    }

    /// One scan tick. Returns actionable games sorted by ascending
    /// factory index. Per-game errors are logged and skipped.
    async fn scan(&self) -> Result<Vec<GameInfo>, ContractError> {
        let game_count = self.factory.game_count().await?;
        if game_count == 0 {
            debug!("factory has no games");
            return Ok(vec![]);
        }

        let scan_start = self.scan_start_index(game_count).await?;

        let results = stream::iter(scan_start..game_count)
            .map(|i| async move { (i, self.read_actionable_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut games = Vec::new();
        for (i, result) in results {
            match result {
                Ok(Some(info)) => games.push(info),
                Ok(None) => {}
                Err(ReadGameError::Classification(e)) => {
                    error!(factory_index = i, error = %e, "unreachable on-chain state; skipping");
                }
                Err(ReadGameError::Contract(e)) => {
                    warn!(factory_index = i, error = %e, "game read failed; skipping");
                }
            }
        }

        games.sort_unstable_by_key(|g| g.factory_index);
        Ok(games)
    }

    /// First factory index to evaluate this tick.
    ///
    /// Returns `0` when the registry is at its starting anchor (`anchorGame
    /// == Address::ZERO`) or when the anchor game cannot be located in the
    /// factory; otherwise `anchor_index + 1`. Hits an internal cache when
    /// the anchor address is unchanged from the previous tick, so the
    /// backward `findLatestGames` walk only runs on anchor changes.
    async fn scan_start_index(&self, game_count: u64) -> Result<u64, ContractError> {
        // Empty factory: nothing to scan, nothing to skip.
        if game_count == 0 {
            return Ok(0);
        }

        let snapshot = self.anchor_registry.anchor_snapshot().await?;
        let anchor_game = snapshot.anchor_game;

        // Registry has no anchor yet (fresh deployment, or anchor was reset):
        // nothing to skip, walk the whole factory. Drop any stale cache.
        if anchor_game == Address::ZERO {
            self.anchor_index_cache.lock().expect("anchor_index_cache lock poisoned").take();
            return Ok(0);
        }

        // Fast path: anchor unchanged since last tick, reuse cached index.
        // The `cached_idx < game_count` guard recovers if the factory state
        // shrinks under us (upgrade or on-chain reset).
        if let Some((cached_addr, cached_idx)) =
            *self.anchor_index_cache.lock().expect("anchor_index_cache lock poisoned")
            && cached_addr == anchor_game
            && cached_idx < game_count
        {
            return Ok(cached_idx + 1);
        }

        // Slow path: anchor changed (or first call); run the batched
        // backward lookup, then refresh the cache with the result (Some
        // on hit, None on miss to invalidate stale entries).
        let found = self.find_anchor_index(anchor_game, game_count).await?;
        *self.anchor_index_cache.lock().expect("anchor_index_cache lock poisoned") =
            found.map(|idx| (anchor_game, idx));

        // Anchor not in factory: log loudly and rescan everything rather
        // than risk silently missing games.
        Ok(found.map_or_else(
            || {
                warn!(%anchor_game, game_count, "anchor game not found in factory; scanning from 0");
                0
            },
            |idx| idx + 1,
        ))
    }

    /// Backward batched lookup of `anchor_game`'s factory index using
    /// `findLatestGames`. Returns `None` once the search has walked past
    /// index 0 without a hit.
    ///
    /// In practice the loop runs once: the anchor advances by ~1 game per
    /// finalization so it sits within the first batch from the tail. The
    /// loop is a safety net for the edge case where the anchor is more
    /// than `ANCHOR_LOOKUP_BATCH` matching games behind.
    async fn find_anchor_index(
        &self,
        anchor_game: Address,
        game_count: u64,
    ) -> Result<Option<u64>, ContractError> {
        let mut start = game_count - 1;
        loop {
            // Cap the request at the number of indices remaining so we
            // never ask the contract to walk into negative territory.
            let n = Self::ANCHOR_LOOKUP_BATCH.min(start + 1);
            let batch = self.factory.find_latest_games(self.game_type, start, n).await?;

            // Hit: anchor sits inside this batch, return its factory index.
            if let Some(&(idx, _)) = batch.iter().find(|(_, proxy)| *proxy == anchor_game) {
                return Ok(Some(idx));
            }

            // Short batch means the on-chain loop walked past index 0
            // without hitting `n` matches, so the anchor is not in the
            // factory.
            if (batch.len() as u64) < n {
                return Ok(None);
            }

            let lowest =
                batch.iter().map(|(idx, _)| *idx).min().expect("batch is non-empty (len >= n > 0)");

            // Defensive: the contract must only return indices ≤ start.
            // Anything else means a contract bug; fail loudly rather than
            // risk an unbounded loop.
            if lowest > start {
                return Err(ContractError::Validation(format!(
                    "findLatestGames returned out-of-range index {lowest} > start {start}"
                )));
            }

            // Reached the bottom of the factory without finding the anchor.
            if lowest == 0 {
                return Ok(None);
            }

            // Continue the backward walk just below the lowest index seen.
            start = lowest - 1;
        }
    }

    /// Reads a game and returns `Some(GameInfo)` only when it is
    /// actionable. Non-actionable cases (`status != IN_PROGRESS`,
    /// [`GameSituation::Terminal`], [`GameSituation::TeeNullifiedDuringChallenge`])
    /// resolve to `None` without fetching the heavier fields.
    async fn read_actionable_game(&self, index: u64) -> Result<Option<GameInfo>, ReadGameError> {
        let factory_entry = self.factory.game_at_index(index).await?;
        let proxy = factory_entry.proxy;

        let status = self.verifier.status(proxy).await?;
        if status != GameStatus::InProgress {
            debug!(game = %proxy, factory_index = index, %status, "game not in progress");
            return Ok(None);
        }

        let (tee_prover, zk_prover, countered) = tokio::try_join!(
            self.verifier.tee_prover(proxy),
            self.verifier.zk_prover(proxy),
            self.verifier.countered_index(proxy),
        )?;

        let situation = GameSituation::classify(tee_prover, zk_prover, countered)?;

        match situation {
            GameSituation::TeeOnly
            | GameSituation::ZkOnly
            | GameSituation::BothProven
            | GameSituation::UnderChallenge { .. } => {}
            GameSituation::TeeNullifiedDuringChallenge | GameSituation::Terminal => {
                debug!(game = %proxy, factory_index = index, %situation, "game not actionable");
                return Ok(None);
            }
        }

        let ((info, starting_block_number, l1_head, intermediate_roots), interval) = tokio::try_join!(
            async {
                tokio::try_join!(
                    self.verifier.game_info(proxy),
                    self.verifier.starting_block_number(proxy),
                    self.verifier.l1_head(proxy),
                    self.verifier.intermediate_output_roots(proxy),
                )
            },
            self.resolve_intermediate_block_interval(factory_entry.game_type),
        )?;

        Ok(Some(GameInfo {
            address: proxy,
            factory_index: index,
            root_claim: info.root_claim,
            l1_head,
            l2_block_number: info.l2_block_number,
            starting_l2_block: starting_block_number,
            intermediate_roots: intermediate_roots.into_boxed_slice(),
            intermediate_block_interval: interval,
            situation,
        }))
    }

    /// Cached lookup of `INTERMEDIATE_BLOCK_INTERVAL` per `(game_type,
    /// impl_address)`. The cache key includes `impl_address` so that a
    /// governance `setImplementation` call invalidates the cache
    /// automatically.
    async fn resolve_intermediate_block_interval(
        &self,
        game_type: u32,
    ) -> Result<u64, ContractError> {
        // Resolve to the current impl so a governance `setImplementation`
        // naturally invalidates the cache (different impl gives a new key).
        let impl_address = self.factory.game_impls(game_type).await?;

        // No impl registered for this `game_type`: misconfigured factory
        // or unknown type; surface it instead of silently caching ZERO.
        if impl_address == Address::ZERO {
            return Err(ContractError::Validation(format!(
                "no game implementation registered for game_type {game_type}"
            )));
        }

        let key = (game_type, impl_address);

        // Fast path: value is immutable per impl, so a hit is always
        // current; no validity check needed beyond key match.
        if let Some(&cached) =
            self.interval_cache.lock().expect("interval_cache lock poisoned").get(&key)
        {
            return Ok(cached);
        }

        // Slow path: read once from the impl contract, store, return.
        let interval = self.verifier.read_intermediate_block_interval(impl_address).await?;
        self.interval_cache.lock().expect("interval_cache lock poisoned").insert(key, interval);
        Ok(interval)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const TEE: Address = address!("00000000000000000000000000000000000000aa");
    const ZK: Address = address!("00000000000000000000000000000000000000bb");
    const ZERO: Address = Address::ZERO;

    #[test]
    fn classify_tee_only() {
        assert_eq!(GameSituation::classify(TEE, ZERO, 0), Ok(GameSituation::TeeOnly));
    }

    #[test]
    fn classify_zk_only() {
        assert_eq!(GameSituation::classify(ZERO, ZK, 0), Ok(GameSituation::ZkOnly));
    }

    #[test]
    fn classify_under_challenge_index_zero() {
        assert_eq!(
            GameSituation::classify(TEE, ZK, 1),
            Ok(GameSituation::UnderChallenge { challenged_index: 0 }),
        );
    }

    #[test]
    fn classify_under_challenge_higher_index() {
        assert_eq!(
            GameSituation::classify(TEE, ZK, 7),
            Ok(GameSituation::UnderChallenge { challenged_index: 6 }),
        );
    }

    #[test]
    fn classify_both_proven() {
        assert_eq!(GameSituation::classify(TEE, ZK, 0), Ok(GameSituation::BothProven));
    }

    #[test]
    fn classify_tee_nullified_during_challenge() {
        assert_eq!(
            GameSituation::classify(ZERO, ZK, 1),
            Ok(GameSituation::TeeNullifiedDuringChallenge),
        );
    }

    #[test]
    fn classify_terminal() {
        assert_eq!(GameSituation::classify(ZERO, ZERO, 0), Ok(GameSituation::Terminal));
    }

    #[test]
    fn classify_unreachable_tee_with_challenge_no_zk() {
        assert_eq!(
            GameSituation::classify(TEE, ZERO, 1),
            Err(ClassifyError::Unreachable { tee_prover: TEE, zk_prover: ZERO, countered: 1 }),
        );
    }

    #[test]
    fn classify_unreachable_no_provers_with_challenge() {
        assert_eq!(
            GameSituation::classify(ZERO, ZERO, 5),
            Err(ClassifyError::Unreachable { tee_prover: ZERO, zk_prover: ZERO, countered: 5 }),
        );
    }

    #[test]
    fn display_returns_variant_name() {
        assert_eq!(format!("{}", GameSituation::TeeOnly), "TeeOnly");
        assert_eq!(format!("{}", GameSituation::ZkOnly), "ZkOnly");
        assert_eq!(format!("{}", GameSituation::BothProven), "BothProven");
        assert_eq!(
            format!("{}", GameSituation::UnderChallenge { challenged_index: 5 }),
            "UnderChallenge",
        );
        assert_eq!(
            format!("{}", GameSituation::TeeNullifiedDuringChallenge),
            "TeeNullifiedDuringChallenge",
        );
        assert_eq!(format!("{}", GameSituation::Terminal), "Terminal");
    }

    mod discovery {
        use crate::test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockGameState,
            addr, factory_game,
        };

        use super::*;

        const IMPL_ADDR: Address = address!("00000000000000000000000000000000000000ff");
        const GAME_TYPE: u32 = 0;

        struct Fixture {
            factory: Arc<MockDisputeGameFactory>,
            verifier: Arc<MockAggregateVerifier>,
            anchor: Arc<MockAnchorStateRegistry>,
            discovery: GameDiscovery,
        }

        impl Fixture {
            fn new() -> Self {
                let factory = Arc::new(MockDisputeGameFactory::new());
                factory.set_impl(GAME_TYPE, IMPL_ADDR);
                let verifier = Arc::new(MockAggregateVerifier::new());
                let anchor = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
                let discovery = GameDiscovery::new(
                    Arc::<MockDisputeGameFactory>::clone(&factory),
                    Arc::<MockAggregateVerifier>::clone(&verifier),
                    Arc::<MockAnchorStateRegistry>::clone(&anchor),
                    GAME_TYPE,
                );
                Self { factory, verifier, anchor, discovery }
            }

            fn push_game(&self, index: u64, state: MockGameState) -> Address {
                // Pad gaps with Terminal placeholders so factory indices line up.
                let current = self.factory.games.lock().expect("games lock poisoned").len() as u64;
                for i in current..index {
                    let placeholder = factory_game(i, GAME_TYPE);
                    self.factory.push(placeholder);
                    self.verifier.set_game(
                        placeholder.proxy,
                        MockGameState::in_progress(Address::ZERO, Address::ZERO, 0),
                    );
                }
                let game = factory_game(index, GAME_TYPE);
                self.factory.push(game);
                self.verifier.set_game(game.proxy, state);
                game.proxy
            }
        }

        #[tokio::test]
        async fn read_actionable_returns_none_when_not_in_progress() {
            let fx = Fixture::new();
            let mut state = MockGameState::in_progress(TEE, ZERO, 0);
            state.status = GameStatus::DefenderWins;
            fx.push_game(0, state);

            assert_eq!(fx.discovery.read_actionable_game(0).await.unwrap(), None);
        }

        #[tokio::test]
        async fn read_actionable_returns_none_for_terminal() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(ZERO, ZERO, 0));

            assert_eq!(fx.discovery.read_actionable_game(0).await.unwrap(), None);
        }

        #[tokio::test]
        async fn read_actionable_returns_none_for_tee_nullified_during_challenge() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(ZERO, ZK, 1));

            assert_eq!(fx.discovery.read_actionable_game(0).await.unwrap(), None);
        }

        #[tokio::test]
        async fn read_actionable_returns_some_for_tee_only() {
            let fx = Fixture::new();
            let proxy = fx.push_game(3, MockGameState::in_progress(TEE, ZERO, 0));

            let info =
                fx.discovery.read_actionable_game(3).await.unwrap().expect("expected actionable");
            assert_eq!(info.address, proxy);
            assert_eq!(info.factory_index, 3);
            assert_eq!(info.situation, GameSituation::TeeOnly);
        }

        #[tokio::test]
        async fn read_actionable_returns_some_for_zk_only() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(ZERO, ZK, 0));

            let info =
                fx.discovery.read_actionable_game(0).await.unwrap().expect("expected actionable");
            assert_eq!(info.situation, GameSituation::ZkOnly);
        }

        #[tokio::test]
        async fn read_actionable_returns_some_for_both_proven() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZK, 0));

            let info =
                fx.discovery.read_actionable_game(0).await.unwrap().expect("expected actionable");
            assert_eq!(info.situation, GameSituation::BothProven);
        }

        #[tokio::test]
        async fn read_actionable_returns_some_for_under_challenge_with_index() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZK, 4));

            let info =
                fx.discovery.read_actionable_game(0).await.unwrap().expect("expected actionable");
            assert_eq!(info.situation, GameSituation::UnderChallenge { challenged_index: 3 });
        }

        #[tokio::test]
        async fn read_actionable_returns_classification_error_for_unreachable() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZERO, 1));

            let err = fx.discovery.read_actionable_game(0).await.unwrap_err();
            assert!(matches!(
                err,
                ReadGameError::Classification(ClassifyError::Unreachable { .. })
            ));
        }

        #[tokio::test]
        async fn read_actionable_propagates_contract_error() {
            let fx = Fixture::new();
            // Index 0 is empty, so game_at_index returns Validation error.
            let err = fx.discovery.read_actionable_game(0).await.unwrap_err();
            assert!(matches!(err, ReadGameError::Contract(_)));
        }

        #[tokio::test]
        async fn read_actionable_populates_all_fields() {
            let fx = Fixture::new();
            let roots = vec![B256::repeat_byte(0xAA), B256::repeat_byte(0xBB)];
            let mut state = MockGameState::in_progress(TEE, ZERO, 0);
            state.root_claim = roots[1];
            state.l2_block_number = 200;
            state.starting_block_number = 100;
            state.l1_head = B256::repeat_byte(0x11);
            state.intermediate_output_roots = roots.clone();
            fx.verifier.set_interval(50);
            let proxy = fx.push_game(7, state);

            let info =
                fx.discovery.read_actionable_game(7).await.unwrap().expect("expected actionable");
            assert_eq!(info.address, proxy);
            assert_eq!(info.factory_index, 7);
            assert_eq!(info.root_claim, roots[1]);
            assert_eq!(info.l1_head, B256::repeat_byte(0x11));
            assert_eq!(info.l2_block_number, 200);
            assert_eq!(info.starting_l2_block, 100);
            assert_eq!(info.intermediate_roots.as_ref(), roots.as_slice());
            assert_eq!(info.intermediate_block_interval, 50);
            assert_eq!(info.situation, GameSituation::TeeOnly);
        }

        #[tokio::test]
        async fn scan_start_index_returns_zero_when_anchor_is_zero_address() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZERO, 0));

            assert_eq!(fx.discovery.scan_start_index(1).await.unwrap(), 0);
        }

        #[tokio::test]
        async fn scan_start_index_returns_zero_when_factory_empty() {
            let fx = Fixture::new();
            fx.anchor.set_anchor_game(addr(42));

            assert_eq!(fx.discovery.scan_start_index(0).await.unwrap(), 0);
        }

        #[tokio::test]
        async fn scan_start_index_returns_anchor_plus_one() {
            let fx = Fixture::new();
            for i in 0..5 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(2));

            assert_eq!(fx.discovery.scan_start_index(5).await.unwrap(), 3);
        }

        #[tokio::test]
        async fn scan_start_index_returns_zero_when_anchor_not_found() {
            let fx = Fixture::new();
            for i in 0..3 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(99));

            assert_eq!(fx.discovery.scan_start_index(3).await.unwrap(), 0);
        }

        #[tokio::test]
        async fn scan_start_index_handles_anchor_at_last_index() {
            let fx = Fixture::new();
            for i in 0..3 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(2));

            assert_eq!(fx.discovery.scan_start_index(3).await.unwrap(), 3);
        }

        #[tokio::test]
        async fn scan_start_index_propagates_anchor_snapshot_error() {
            let fx = Fixture::new();
            fx.anchor.set_fail_snapshot(true);

            assert!(fx.discovery.scan_start_index(1).await.is_err());
        }

        #[tokio::test]
        async fn scan_start_index_caches_anchor_index_across_ticks() {
            let fx = Fixture::new();
            for i in 0..5 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(2));

            // First call: walks the factory and finds anchor.
            assert_eq!(fx.discovery.scan_start_index(5).await.unwrap(), 3);

            // Mutate the underlying mock factory to "lose" index 2. A cached
            // hit must still resolve to 3 without re-walking the factory.
            fx.factory.games.lock().expect("games lock poisoned").drain(0..2);
            assert_eq!(fx.discovery.scan_start_index(5).await.unwrap(), 3);
        }

        #[tokio::test]
        async fn scan_start_index_invalidates_cache_when_anchor_changes() {
            let fx = Fixture::new();
            for i in 0..5 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(1));
            assert_eq!(fx.discovery.scan_start_index(5).await.unwrap(), 2);

            fx.anchor.set_anchor_game(addr(3));
            assert_eq!(fx.discovery.scan_start_index(5).await.unwrap(), 4);
        }

        #[tokio::test]
        async fn scan_start_index_clears_cache_when_anchor_resets_to_zero() {
            let fx = Fixture::new();
            for i in 0..3 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(1));
            assert_eq!(fx.discovery.scan_start_index(3).await.unwrap(), 2);

            fx.anchor.set_anchor_game(Address::ZERO);
            assert_eq!(fx.discovery.scan_start_index(3).await.unwrap(), 0);
        }

        #[tokio::test]
        async fn scan_returns_empty_when_factory_empty() {
            let fx = Fixture::new();
            assert!(fx.discovery.scan().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn scan_returns_only_actionable_games() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZERO, 0)); // TeeOnly: actionable
            fx.push_game(1, MockGameState::in_progress(ZERO, ZERO, 0)); // Terminal: skip
            fx.push_game(2, MockGameState::in_progress(ZERO, ZK, 1)); // TeeNullifiedDuringChallenge: skip
            fx.push_game(3, MockGameState::in_progress(TEE, ZK, 2)); // UnderChallenge: actionable

            let games = fx.discovery.scan().await.unwrap();
            assert_eq!(games.len(), 2);
            assert_eq!(games[0].factory_index, 0);
            assert_eq!(games[0].situation, GameSituation::TeeOnly);
            assert_eq!(games[1].factory_index, 3);
            assert_eq!(games[1].situation, GameSituation::UnderChallenge { challenged_index: 1 },);
        }

        #[tokio::test]
        async fn scan_skips_games_pre_anchor() {
            let fx = Fixture::new();
            for i in 0..5 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }
            fx.anchor.set_anchor_game(addr(2));

            let games = fx.discovery.scan().await.unwrap();
            assert_eq!(games.len(), 2);
            assert_eq!(games[0].factory_index, 3);
            assert_eq!(games[1].factory_index, 4);
        }

        #[tokio::test]
        async fn scan_continues_when_one_game_classification_unreachable() {
            let fx = Fixture::new();
            fx.push_game(0, MockGameState::in_progress(TEE, ZERO, 0)); // ok
            fx.push_game(1, MockGameState::in_progress(TEE, ZERO, 1)); // unreachable: log + skip
            fx.push_game(2, MockGameState::in_progress(ZERO, ZK, 0)); // ok

            let games = fx.discovery.scan().await.unwrap();
            assert_eq!(games.len(), 2);
            assert_eq!(games[0].factory_index, 0);
            assert_eq!(games[1].factory_index, 2);
        }

        #[tokio::test]
        async fn scan_returns_games_sorted_by_factory_index() {
            let fx = Fixture::new();
            for i in 0..6 {
                fx.push_game(i, MockGameState::in_progress(TEE, ZERO, 0));
            }

            let games = fx.discovery.scan().await.unwrap();
            assert_eq!(
                games.iter().map(|g| g.factory_index).collect::<Vec<_>>(),
                vec![0, 1, 2, 3, 4, 5]
            );
        }

        #[tokio::test]
        async fn resolve_intermediate_block_interval_caches_per_impl() {
            let fx = Fixture::new();
            fx.verifier.set_interval(42);

            let v1 = fx.discovery.resolve_intermediate_block_interval(GAME_TYPE).await.unwrap();
            // Mutate the underlying mock value: a cached read should still return the original.
            fx.verifier.set_interval(99);
            let v2 = fx.discovery.resolve_intermediate_block_interval(GAME_TYPE).await.unwrap();

            assert_eq!(v1, 42);
            assert_eq!(v2, 42);
        }

        #[tokio::test]
        async fn resolve_intermediate_block_interval_errors_when_impl_unset() {
            let fx = Fixture::new();
            // GAME_TYPE 1 has no impl registered.
            let err = fx.discovery.resolve_intermediate_block_interval(1).await.unwrap_err();
            assert!(matches!(err, ContractError::Validation(_)));
        }
    }

    mod discovery_run {
        use crate::test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockGameState,
            factory_game,
        };

        use super::*;

        const IMPL_ADDR: Address = address!("00000000000000000000000000000000000000ff");
        const GAME_TYPE: u32 = 0;

        struct Setup {
            factory: Arc<MockDisputeGameFactory>,
            verifier: Arc<MockAggregateVerifier>,
            anchor: Arc<MockAnchorStateRegistry>,
            discovery: GameDiscovery,
        }

        fn setup() -> Setup {
            let factory = Arc::new(MockDisputeGameFactory::new());
            factory.set_impl(GAME_TYPE, IMPL_ADDR);
            let verifier = Arc::new(MockAggregateVerifier::new());
            let anchor = Arc::new(MockAnchorStateRegistry::new(Address::ZERO));
            let discovery = GameDiscovery::new(
                Arc::<MockDisputeGameFactory>::clone(&factory),
                Arc::<MockAggregateVerifier>::clone(&verifier),
                Arc::<MockAnchorStateRegistry>::clone(&anchor),
                GAME_TYPE,
            );
            Setup { factory, verifier, anchor, discovery }
        }

        fn push_actionable(setup: &Setup, index: u64) {
            let game = factory_game(index, GAME_TYPE);
            setup.factory.push(game);
            setup.verifier.set_game(game.proxy, MockGameState::in_progress(TEE, ZERO, 0));
        }

        #[tokio::test(start_paused = true)]
        async fn run_exits_on_cancel_before_first_tick() {
            let Setup { discovery, .. } = setup();
            let (tx, _rx) = mpsc::channel(1);
            let cancel = CancellationToken::new();

            let handle = tokio::spawn(discovery.run(tx, Duration::from_secs(60), cancel.clone()));
            cancel.cancel();
            handle.await.expect("run must exit cleanly");
        }

        #[tokio::test(start_paused = true)]
        async fn run_sends_games_after_each_poll_interval() {
            let setup = setup();
            push_actionable(&setup, 0);

            let (tx, mut rx) = mpsc::channel(10);
            let cancel = CancellationToken::new();
            let interval = Duration::from_secs(1);

            let handle = tokio::spawn(setup.discovery.run(tx, interval, cancel.clone()));

            tokio::time::advance(interval).await;
            let g1 = rx.recv().await.expect("first tick should send a game");
            assert_eq!(g1.factory_index, 0);

            tokio::time::advance(interval).await;
            let g2 = rx.recv().await.expect("second tick should send a game");
            assert_eq!(g2.factory_index, 0);

            cancel.cancel();
            handle.await.expect("run must exit cleanly");
        }

        #[tokio::test(start_paused = true)]
        async fn run_continues_after_scan_error() {
            let setup = setup();
            push_actionable(&setup, 0);
            setup.anchor.set_fail_snapshot(true);

            let (tx, mut rx) = mpsc::channel(10);
            let cancel = CancellationToken::new();
            let interval = Duration::from_secs(1);

            let handle = tokio::spawn(setup.discovery.run(tx, interval, cancel.clone()));

            // A few failed ticks while anchor is unhealthy.
            for _ in 0..3 {
                tokio::time::advance(interval).await;
                tokio::task::yield_now().await;
            }
            assert!(rx.try_recv().is_err(), "no games while anchor failing");

            // Recover and verify the next tick succeeds.
            setup.anchor.set_fail_snapshot(false);
            tokio::time::advance(interval).await;
            let game = rx.recv().await.expect("scan should succeed after anchor recovery");
            assert_eq!(game.factory_index, 0);

            cancel.cancel();
            handle.await.expect("run must exit cleanly");
        }

        #[tokio::test(start_paused = true)]
        async fn run_exits_when_receiver_dropped() {
            let setup = setup();
            push_actionable(&setup, 0);

            let (tx, rx) = mpsc::channel(1);
            let cancel = CancellationToken::new();
            let interval = Duration::from_secs(1);

            let handle = tokio::spawn(setup.discovery.run(tx, interval, cancel.clone()));

            drop(rx);
            tokio::time::advance(interval).await;

            handle.await.expect("run must exit cleanly when receiver dropped");
        }
    }
}
