//! Bond candidate discovery.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy_primitives::Address;
use base_proof_contracts::{AggregateVerifierClient, ContractError, DisputeGameFactoryClient};
use futures::{StreamExt, stream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::Metrics;

/// A game forwarded to the bond pool for claiming.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BondCandidate {
    /// Game proxy address.
    pub game_address: Address,
    /// Address (one of `claim_addresses`) that will receive the bond.
    pub bond_recipient: Address,
}

/// Periodic discovery of games whose bond pays out to one of
/// `claim_addresses`.
pub struct BondDiscovery {
    /// Source of the factory game count and per-index lookups.
    factory: Arc<dyn DisputeGameFactoryClient>,
    /// Reads `bondRecipient` and `zkProver` per game.
    verifier: Arc<dyn AggregateVerifierClient>,
    /// Recipient addresses the discovery emits candidates for.
    claim_addresses: HashSet<Address>,
    /// Time window scanned each tick (relative to now).
    max_age: Duration,
    /// Sleep between scan ticks.
    poll_interval: Duration,
}

impl std::fmt::Debug for BondDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondDiscovery")
            .field("claim_addresses", &self.claim_addresses)
            .field("max_age", &self.max_age)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl BondDiscovery {
    /// Maximum number of concurrent RPC reads per scan tick.
    const SCAN_CONCURRENCY: usize = 32;

    /// Builds a discovery against the given clients and configuration.
    pub fn new(
        factory: Arc<dyn DisputeGameFactoryClient>,
        verifier: Arc<dyn AggregateVerifierClient>,
        claim_addresses: HashSet<Address>,
        max_age: Duration,
        poll_interval: Duration,
    ) -> Self {
        Self { factory, verifier, claim_addresses, max_age, poll_interval }
    }

    /// Periodic scan loop. Sleeps `poll_interval` between ticks, sends
    /// each candidate on `tx`, and exits when `cancel` fires or the
    /// receiver is dropped. Per-tick errors are logged. Returns
    /// without scanning when `claim_addresses` is empty.
    pub async fn run(self, tx: mpsc::Sender<BondCandidate>, cancel: CancellationToken) {
        if self.claim_addresses.is_empty() {
            debug!("no claim addresses configured, bond discovery disabled");
            return;
        }

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(self.poll_interval) => {}
            }

            let now_secs = SystemTime::UNIX_EPOCH
                .elapsed()
                .expect("system clock is before UNIX_EPOCH")
                .as_secs();

            Metrics::bond_scan_ticks_total().increment(1);
            match self.scan(now_secs).await {
                Ok(candidates) => {
                    for candidate in candidates {
                        if tx.send(candidate).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Metrics::bond_scan_errors_total().increment(1);
                    warn!(error = %e, "bond scan failed; will retry next tick");
                }
            }
        }
    }

    /// One scan tick. Returns matching candidates from games created
    /// within `max_age` of `now_secs`.
    async fn scan(&self, now_secs: u64) -> Result<Vec<BondCandidate>, ContractError> {
        let game_count = self.factory.game_count().await?;
        if game_count == 0 {
            debug!("factory has no games");
            Metrics::bonds_inspected().set(0.0);
            Metrics::bond_candidates().set(0.0);
            return Ok(vec![]);
        }

        let start = self.scan_start_index(game_count, now_secs).await?;
        if start >= game_count {
            Metrics::bonds_inspected().set(0.0);
            Metrics::bond_candidates().set(0.0);
            return Ok(vec![]);
        }

        let candidates = self.scan_range(start, game_count).await;
        Metrics::bonds_inspected().set((game_count - start) as f64);
        #[allow(clippy::cast_precision_loss)]
        Metrics::bond_candidates().set(candidates.len() as f64);
        Ok(candidates)
    }

    /// First factory index whose `gameAtIndex.timestamp >= now - max_age`.
    /// Returns `game_count` when every game is older.
    async fn scan_start_index(&self, game_count: u64, now_secs: u64) -> Result<u64, ContractError> {
        let cutoff = now_secs.saturating_sub(self.max_age.as_secs());
        let mut lo = 0u64;
        let mut hi = game_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let game = self.factory.game_at_index(mid).await?;
            if game.timestamp >= cutoff {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(lo)
    }

    /// Evaluates factory indices in `[start, end)` concurrently. Per-game
    /// errors are logged and skipped.
    async fn scan_range(&self, start: u64, end: u64) -> Vec<BondCandidate> {
        stream::iter(start..end)
            .map(|i| async move { (i, self.read_game(i).await) })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .filter_map(|(i, result)| async move {
                match result {
                    Ok(Some(candidate)) => Some(candidate),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(factory_index = i, error = %e, "game read failed; skipping");
                        None
                    }
                }
            })
            .collect()
            .await
    }

    /// Returns `Some(BondCandidate)` when `bondRecipient` or `zkProver`
    /// for the game at `index` is in `claim_addresses`.
    async fn read_game(&self, index: u64) -> Result<Option<BondCandidate>, ContractError> {
        let game = self.factory.game_at_index(index).await?;
        let proxy = game.proxy;

        let (bond_recipient, zk_prover) =
            tokio::try_join!(self.verifier.bond_recipient(proxy), self.verifier.zk_prover(proxy),)?;

        let recipient = if self.claim_addresses.contains(&bond_recipient) {
            bond_recipient
        } else if !zk_prover.is_zero() && self.claim_addresses.contains(&zk_prover) {
            zk_prover
        } else {
            return Ok(None);
        };

        Ok(Some(BondCandidate { game_address: proxy, bond_recipient: recipient }))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockGameState, addr, factory_game,
    };

    const CLAIM_A: Address = address!("00000000000000000000000000000000000000a1");
    const CLAIM_B: Address = address!("00000000000000000000000000000000000000a2");
    const STRANGER: Address = address!("00000000000000000000000000000000000000ee");
    const GAME_TYPE: u32 = 0;

    fn claim_set(addrs: &[Address]) -> HashSet<Address> {
        addrs.iter().copied().collect()
    }

    struct Fixture {
        factory: Arc<MockDisputeGameFactory>,
        verifier: Arc<MockAggregateVerifier>,
        discovery: BondDiscovery,
    }

    impl Fixture {
        fn new(claim_addresses: HashSet<Address>) -> Self {
            Self::with_max_age(claim_addresses, Duration::from_secs(86_400 * 21))
        }

        fn with_max_age(claim_addresses: HashSet<Address>, max_age: Duration) -> Self {
            let factory = Arc::new(MockDisputeGameFactory::new());
            let verifier = Arc::new(MockAggregateVerifier::new());
            let discovery = BondDiscovery::new(
                Arc::<MockDisputeGameFactory>::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
                Arc::<MockAggregateVerifier>::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
                claim_addresses,
                max_age,
                Duration::from_secs(60),
            );
            Self { factory, verifier, discovery }
        }

        /// Pushes a game at the next factory index with the given state and
        /// returns its proxy address.
        fn push_game(&self, state: MockGameState) -> Address {
            let index = self.factory.games.lock().expect("games lock poisoned").len() as u64;
            let entry = factory_game(index, GAME_TYPE);
            self.factory.push(entry);
            self.verifier.set_game(entry.proxy, state);
            entry.proxy
        }

        /// Pushes a game whose factory entry uses an explicit timestamp
        /// (for binary-search tests).
        fn push_game_at(&self, timestamp: u64, state: MockGameState) -> Address {
            let index = self.factory.games.lock().expect("games lock poisoned").len() as u64;
            let entry = base_proof_contracts::GameAtIndex {
                game_type: GAME_TYPE,
                timestamp,
                proxy: addr(index + 1),
            };
            self.factory.push(entry);
            self.verifier.set_game(entry.proxy, state);
            entry.proxy
        }
    }

    fn state_with_recipient(bond_recipient: Address) -> MockGameState {
        let mut state = MockGameState::in_progress(Address::ZERO, Address::ZERO, 0);
        state.bond_recipient = bond_recipient;
        state
    }

    fn state_with_zk_prover(zk_prover: Address) -> MockGameState {
        MockGameState::in_progress(Address::ZERO, zk_prover, 0)
    }

    // ── read_game ────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_game_returns_candidate_on_bond_recipient_match() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        let proxy = fx.push_game(state_with_recipient(CLAIM_A));

        let candidate = fx.discovery.read_game(0).await.unwrap().expect("expected match");
        assert_eq!(candidate.game_address, proxy);
        assert_eq!(candidate.bond_recipient, CLAIM_A);
    }

    #[tokio::test]
    async fn read_game_returns_candidate_on_zk_prover_match_pre_resolve() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        // bondRecipient still ZERO (pre-resolve), but zkProver is ours.
        let proxy = fx.push_game(state_with_zk_prover(CLAIM_A));

        let candidate = fx.discovery.read_game(0).await.unwrap().expect("expected match");
        assert_eq!(candidate.game_address, proxy);
        assert_eq!(candidate.bond_recipient, CLAIM_A);
    }

    #[tokio::test]
    async fn read_game_prefers_bond_recipient_over_zk_prover_when_both_match() {
        let fx = Fixture::new(claim_set(&[CLAIM_A, CLAIM_B]));
        let mut state = state_with_zk_prover(CLAIM_B);
        state.bond_recipient = CLAIM_A;
        fx.push_game(state);

        let candidate = fx.discovery.read_game(0).await.unwrap().expect("expected match");
        assert_eq!(candidate.bond_recipient, CLAIM_A);
    }

    #[tokio::test]
    async fn read_game_returns_none_when_neither_field_in_claim_set() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        fx.push_game(state_with_zk_prover(STRANGER));

        assert_eq!(fx.discovery.read_game(0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_game_returns_none_when_zk_prover_zero_and_recipient_unmatched() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        fx.push_game(state_with_recipient(STRANGER));

        assert_eq!(fx.discovery.read_game(0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_game_propagates_factory_error_for_missing_index() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        // No game pushed at index 0.
        assert!(fx.discovery.read_game(0).await.is_err());
    }

    // ── scan_start_index ──────────────────────────────────────────────

    #[tokio::test]
    async fn scan_start_index_returns_zero_when_all_games_in_window() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(1_000));
        for ts in [100u64, 200, 300, 400] {
            fx.push_game_at(ts, state_with_recipient(STRANGER));
        }

        // now = 500, cutoff = 500 - 1000 saturates to 0, so all timestamps qualify.
        assert_eq!(fx.discovery.scan_start_index(4, 500).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn scan_start_index_returns_game_count_when_all_too_old() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(50));
        for ts in [100u64, 200, 300] {
            fx.push_game_at(ts, state_with_recipient(STRANGER));
        }

        // now = 1000, cutoff = 950, no game qualifies.
        assert_eq!(fx.discovery.scan_start_index(3, 1_000).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn scan_start_index_finds_first_index_in_window() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(150));
        for ts in [100u64, 200, 300, 400, 500] {
            fx.push_game_at(ts, state_with_recipient(STRANGER));
        }

        // now = 500, cutoff = 350, expect indices [3, 4] qualify, first = 3.
        assert_eq!(fx.discovery.scan_start_index(5, 500).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn scan_start_index_handles_zero_count() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(1_000));
        assert_eq!(fx.discovery.scan_start_index(0, 1_000).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn scan_start_index_handles_single_game_inside() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(100));
        fx.push_game_at(450, state_with_recipient(STRANGER));

        assert_eq!(fx.discovery.scan_start_index(1, 500).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn scan_start_index_handles_single_game_outside() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(50));
        fx.push_game_at(100, state_with_recipient(STRANGER));

        assert_eq!(fx.discovery.scan_start_index(1, 500).await.unwrap(), 1);
    }

    // ── scan_range ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn scan_range_returns_only_matches() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        let _miss = fx.push_game(state_with_recipient(STRANGER));
        let hit_a = fx.push_game(state_with_recipient(CLAIM_A));
        let _miss2 = fx.push_game(state_with_zk_prover(STRANGER));
        let hit_zk = fx.push_game(state_with_zk_prover(CLAIM_A));

        let mut candidates = fx.discovery.scan_range(0, 4).await;
        candidates.sort_by_key(|c| c.game_address);

        let mut expected = vec![hit_a, hit_zk];
        expected.sort();
        assert_eq!(candidates.iter().map(|c| c.game_address).collect::<Vec<_>>(), expected,);
    }

    #[tokio::test]
    async fn scan_range_skips_games_with_read_errors() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        let hit = fx.push_game(state_with_recipient(CLAIM_A));
        // Push a factory entry with no matching verifier state to force a read error.
        let orphan = factory_game(1, GAME_TYPE);
        fx.factory.push(orphan);
        let hit2 = fx.push_game(state_with_recipient(CLAIM_A));

        let mut candidates = fx.discovery.scan_range(0, 3).await;
        candidates.sort_by_key(|c| c.game_address);

        let mut expected = vec![hit, hit2];
        expected.sort();
        assert_eq!(candidates.iter().map(|c| c.game_address).collect::<Vec<_>>(), expected,);
    }

    #[tokio::test]
    async fn scan_range_empty_when_start_equals_end() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        fx.push_game(state_with_recipient(CLAIM_A));

        assert!(fx.discovery.scan_range(1, 1).await.is_empty());
    }

    // ── scan (full pipeline) ─────────────────────────────────────────────

    #[tokio::test]
    async fn scan_returns_empty_when_factory_empty() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        assert!(fx.discovery.scan(1_000).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scan_excludes_games_older_than_max_age() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(150));
        // Old game (outside window): should be skipped even though it matches.
        fx.push_game_at(100, state_with_recipient(CLAIM_A));
        // Recent game (inside window): kept.
        let recent = fx.push_game_at(450, state_with_recipient(CLAIM_A));

        let candidates = fx.discovery.scan(500).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].game_address, recent);
    }

    // ── run ──────────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn run_exits_on_cancel_before_first_tick() {
        let fx = Fixture::new(claim_set(&[CLAIM_A]));
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(fx.discovery.run(tx, cancel.clone()));
        cancel.cancel();
        handle.await.expect("run must exit cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn run_exits_immediately_when_no_claim_addresses() {
        let fx = Fixture::new(HashSet::new());
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        // No cancel issued: must exit on its own.
        fx.discovery.run(tx, cancel).await;
    }

    #[tokio::test(start_paused = true)]
    async fn run_sends_candidates_after_each_tick() {
        // Wide max_age so the wall-clock filter inside `run` is a no-op
        // and the mock factory's small timestamps stay eligible.
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(u64::MAX / 2));
        fx.push_game(state_with_recipient(CLAIM_A));

        let (tx, mut rx) = mpsc::channel(10);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(fx.discovery.run(tx, cancel.clone()));

        tokio::time::advance(Duration::from_secs(60)).await;
        let c1 = rx.recv().await.expect("first tick should send a candidate");
        assert_eq!(c1.bond_recipient, CLAIM_A);

        tokio::time::advance(Duration::from_secs(60)).await;
        let c2 = rx.recv().await.expect("second tick should send a candidate");
        assert_eq!(c2.bond_recipient, CLAIM_A);

        cancel.cancel();
        handle.await.expect("run must exit cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn run_exits_when_receiver_dropped() {
        let fx = Fixture::with_max_age(claim_set(&[CLAIM_A]), Duration::from_secs(u64::MAX / 2));
        fx.push_game(state_with_recipient(CLAIM_A));

        let (tx, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(fx.discovery.run(tx, cancel));

        drop(rx);
        tokio::time::advance(Duration::from_secs(60)).await;

        handle.await.expect("run must exit when receiver dropped");
    }
}
