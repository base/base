//! Bond lifecycle management for resolving dispute games and claiming credits.
//! Tracks configured claim addresses via onchain `bondRecipient` and `zkProver`.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, GameStatus, encode_claim_credit_calldata, encode_resolve_calldata,
};
use base_runtime::Clock;
use base_tx_manager::TxManager;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{ChallengeSubmitError, ChallengeSubmitter, ChallengerMetrics};

/// Manages the bond claim lifecycle for dispute games.
///
/// After a successful `challenge()` submission, games are registered here.
/// On each [`poll`](Self::poll) tick the manager checks each tracked game's
/// onchain state and submits the next transaction in the lifecycle.
///
/// When bond claim addresses are configured, the manager also continuously
/// discovers claimable games via [`discover_claimable_games`](Self::discover_claimable_games),
/// rescanning the lookback window to catch games challenged or resolved by
/// other actors.
pub struct BondManager<C: Clock> {
    /// Games being tracked, keyed by proxy address.
    ///
    /// The value is the local monotonic timestamp of a confirmed unlock
    /// transaction. `None` means the onchain state is authoritative.
    tracked: HashMap<Address, Option<Duration>>,
    /// Addresses we are authorized to claim bonds on behalf of.
    claim_addresses: HashSet<Address>,
    /// `DelayedWETH` withdrawal delay (read from contract at init or lazily
    /// resolved on the first poll tick that has a tracked game).
    weth_delay: Option<Duration>,
    /// L1 RPC URL used to instantiate the `DelayedWETH` contract client
    /// when lazily resolving the withdrawal delay.
    l1_rpc_url: url::Url,
    /// Injectable clock providing monotonic time. In production this is
    /// backed by [`TokioRuntime`](base_runtime::TokioRuntime); tests can
    /// substitute a deterministic clock.
    clock: C,
    /// Factory client for querying game indices during bond discovery.
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    /// Number of recent games to scan during bond discovery.
    lookback: u64,
}

impl<C: Clock> std::fmt::Debug for BondManager<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondManager")
            .field("tracked", &self.tracked.len())
            .field("claim_addresses", &self.claim_addresses.len())
            .field("weth_delay", &self.weth_delay)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> BondManager<C> {
    /// Conservative fallback when the onchain `DelayedWETH` delay has not
    /// been read yet. If the real delay is shorter the withdraw will simply
    /// succeed earlier; if longer, the attempt reverts and is retried.
    const DEFAULT_WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// How long to wait before retrying a reverted withdraw attempt.
    const WITHDRAW_REVERT_RETRY_DELAY: Duration = Duration::from_secs(60);

    /// Creates a new bond manager for the given set of claim addresses.
    pub fn new(
        claim_addresses: Vec<Address>,
        l1_rpc_url: url::Url,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
        clock: C,
    ) -> Self {
        let set: HashSet<Address> = claim_addresses.into_iter().collect();
        info!(count = set.len(), "bond manager initialized with claim addresses");
        Self {
            tracked: HashMap::new(),
            claim_addresses: set,
            weth_delay: None,
            l1_rpc_url,
            clock,
            factory_client,
            lookback,
        }
    }

    /// Returns the number of games currently being tracked.
    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// Registers a game for bond tracking if its `bond_recipient` is in the
    /// configured claim addresses.
    ///
    /// Returns `true` if the game was added to tracking.
    pub fn track_game(&mut self, game_address: Address, bond_recipient: Address) -> bool {
        if !self.claim_addresses.contains(&bond_recipient) {
            debug!(
                game = %game_address,
                recipient = %bond_recipient,
                "skipping game — bond recipient not in claim addresses"
            );
            return false;
        }

        let Entry::Vacant(entry) = self.tracked.entry(game_address) else {
            debug!(game = %game_address, "game already tracked for bond claiming");
            return false;
        };

        info!(game = %game_address, recipient = %bond_recipient, "tracking game for bond claiming");
        entry.insert(None);
        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        true
    }

    /// Evaluates a single game for bond tracking eligibility.
    ///
    /// Fetches the game's `bondRecipient` and `zkProver`, matches them
    /// against `claim_addresses`, and returns the game address if it is
    /// eligible for tracking. Returns `None` when the game is not relevant,
    /// already claimed, or an RPC error occurs.
    async fn evaluate_game_for_bonds(
        &self,
        index: u64,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> Option<Address> {
        let game_at = match self.factory_client.game_at_index(index).await {
            Ok(g) => g,
            Err(e) => {
                warn!(index, error = %e, "failed to fetch game at index");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_GAME_FETCH,
                )
                .increment(1);
                return None;
            }
        };

        let game_address = game_at.proxy;

        let (bond_recipient, zk_prover) = match futures::try_join!(
            verifier_client.bond_recipient(game_address),
            verifier_client.zk_prover(game_address),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                debug!(
                    game = %game_address,
                    error = %e,
                    "failed to read bondRecipient/zkProver"
                );
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_BOND_READ,
                )
                .increment(1);
                return None;
            }
        };

        // Check both `bondRecipient` and `zkProver` against the claim
        // addresses. Before `resolve()`, `bondRecipient` is the game
        // creator while `zkProver` is the address that called
        // `challenge()`. After `resolve()`, `bondRecipient` is updated
        // to the `zkProver`. Checking both ensures we recover pre-resolve
        // challenged games.
        if !self.claim_addresses.contains(&bond_recipient)
            && (zk_prover == Address::ZERO || !self.claim_addresses.contains(&zk_prover))
        {
            return None;
        }

        let (bond_claimed, status) = match futures::try_join!(
            verifier_client.bond_claimed(game_address),
            verifier_client.status(game_address),
        ) {
            Ok(state) => state,
            Err(e) => {
                warn!(
                    game = %game_address,
                    error = %e,
                    "failed to read bond claim state"
                );
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_PHASE_READ,
                )
                .increment(1);
                return None;
            }
        };
        if bond_claimed {
            return None;
        }

        // For already-resolved games, verify the current onchain
        // `bondRecipient` is in our claim addresses. Games matched via
        // `zkProver` may have a `bondRecipient` that is not in our
        // claim set (e.g. a game where our challenge was nullified and
        // the bond goes to the game creator). Pre-resolve games are
        // kept — `bondRecipient` will be re-verified after resolve in
        // `try_resolve`.
        if status != GameStatus::InProgress && !self.claim_addresses.contains(&bond_recipient) {
            debug!(
                game = %game_address,
                recipient = %bond_recipient,
                "onchain bondRecipient not in claim addresses \
                 for resolved game, skipping"
            );
            return None;
        }

        Some(game_address)
    }

    /// Discovers claimable games by rescanning the recent lookback window.
    pub async fn discover_claimable_games(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<()> {
        let game_count = self.factory_client.game_count().await?;
        let scan_start = game_count.saturating_sub(self.lookback);
        debug!(
            scan_start,
            scan_end = game_count,
            effective_span = game_count - scan_start,
            game_count,
            tracked = self.tracked.len(),
            "bond discovery scan"
        );

        ChallengerMetrics::bond_discovery_scans_total().increment(1);

        let results: Vec<_> = stream::iter(scan_start..game_count)
            .map(|i| self.evaluate_game_for_bonds(i, verifier_client))
            .buffer_unordered(32)
            .filter_map(std::future::ready)
            .collect()
            .await;

        let tracked_before = self.tracked.len();

        for game_address in results {
            if let Entry::Vacant(entry) = self.tracked.entry(game_address) {
                info!(game = %game_address, "discovered claimable game");
                entry.insert(None);
            }
        }

        let discovered = (self.tracked.len() - tracked_before) as u64;
        if discovered > 0 {
            ChallengerMetrics::bond_discovery_games_found_total().increment(discovered);
            ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
            info!(discovered, tracked = self.tracked.len(), "bond discovery complete");
        }

        Ok(())
    }

    /// Polls all tracked games and advances each through the bond lifecycle.
    ///
    /// Called once per driver tick. Errors on individual games are logged and
    /// do not abort processing of remaining games.
    pub async fn poll<T: TxManager>(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) {
        if self.tracked.is_empty() {
            return;
        }

        // Lazily resolve the DelayedWETH delay if not yet known.
        if self.weth_delay.is_none()
            && let Some(&game_address) = self.tracked.keys().next()
            && let Err(e) = async {
                let weth_address = verifier_client.delayed_weth(game_address).await?;
                let weth_client =
                    DelayedWETHContractClient::new(weth_address, self.l1_rpc_url.clone())?;
                let delay = weth_client.delay().await?;
                info!(delay_secs = delay.as_secs(), "DelayedWETH delay configured");
                self.weth_delay = Some(delay);
                Ok::<(), eyre::Report>(())
            }
            .await
        {
            warn!(error = %e, "failed to read DelayedWETH delay, will retry later");
        }

        let addresses: Vec<Address> = self.tracked.keys().copied().collect();

        for game_address in addresses {
            match self.advance_game(game_address, verifier_client, submitter).await {
                Ok(true) => {
                    self.tracked.remove(&game_address);
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "failed to advance bond lifecycle"
                    );
                }
            }
        }

        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
    }

    async fn advance_game<T: TxManager>(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<bool> {
        let unlocked_at = match self.tracked.get(&game_address).copied() {
            Some(unlocked_at) => unlocked_at,
            None => return Ok(false),
        };

        let (bond_claimed, status, resolved_at, bond_unlocked) = futures::try_join!(
            verifier_client.bond_claimed(game_address),
            verifier_client.status(game_address),
            verifier_client.resolved_at(game_address),
            verifier_client.bond_unlocked(game_address),
        )?;

        if bond_claimed {
            info!(game = %game_address, "bond already claimed");
            ChallengerMetrics::bonds_completed_total().increment(1);
            return Ok(true);
        }

        if status == GameStatus::InProgress {
            return self.try_resolve(game_address, verifier_client, submitter).await;
        }

        if !self.claimable_after_resolve(game_address, verifier_client).await? {
            return Ok(true);
        }

        if let Some(unlocked_at) = unlocked_at
            && !self.withdraw_delay_elapsed(game_address, resolved_at, Some(unlocked_at))
        {
            return Ok(false);
        }

        if !bond_unlocked {
            self.try_unlock(game_address, submitter).await;
            return Ok(false);
        }

        if !self.withdraw_delay_elapsed(game_address, resolved_at, unlocked_at) {
            return Ok(false);
        }

        Ok(self.try_withdraw(game_address, submitter).await)
    }

    /// Attempts to resolve the game by calling `resolve()`.
    ///
    /// After resolution (either by us or by another actor), re-reads the
    /// onchain `bondRecipient` to verify it is still in our claim
    /// addresses. `resolve()` may update `bondRecipient` (e.g. to the
    /// challenger's address on `CHALLENGER_WINS`), so games matched via
    /// `zkProver` before resolution may no longer be claimable by us.
    async fn try_resolve<T: TxManager>(
        &self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<bool> {
        let game_over = verifier_client.game_over(game_address).await?;
        if !game_over {
            debug!(game = %game_address, "game dispute period not yet elapsed");
            return Ok(false);
        }

        info!(game = %game_address, "submitting resolve transaction");
        match submitter.send_bond_tx(game_address, game_address, encode_resolve_calldata()).await {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    tx_hash = %tx_hash,
                    "resolve transaction confirmed"
                );
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    error = %e,
                    "resolve transaction failed, will retry"
                );
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                return Ok(false);
            }
        }

        if !self.claimable_after_resolve(game_address, verifier_client).await? {
            return Ok(true);
        }

        Ok(false)
    }

    async fn claimable_after_resolve(
        &self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<bool> {
        let bond_recipient = verifier_client.bond_recipient(game_address).await?;
        if !self.claim_addresses.contains(&bond_recipient) {
            info!(
                game = %game_address,
                recipient = %bond_recipient,
                "bond recipient not in claim addresses after resolve, removing from tracking"
            );
            ChallengerMetrics::bonds_not_claimable_total().increment(1);
            return Ok(false);
        }

        Ok(true)
    }

    async fn try_unlock<T: TxManager>(
        &mut self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) {
        match self.send_claim_credit(game_address, submitter, "unlock").await {
            Ok(()) => {
                self.tracked.insert(game_address, Some(self.clock.now()));
            }
            Err(e) => Self::warn_claim_credit_retry(game_address, &e, "unlock"),
        }
    }

    async fn try_withdraw<T: TxManager>(
        &mut self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) -> bool {
        match self.send_claim_credit(game_address, submitter, "withdraw").await {
            Ok(()) => {
                ChallengerMetrics::bonds_completed_total().increment(1);
                true
            }
            Err(e) => {
                if matches!(&e, ChallengeSubmitError::TxReverted { .. }) {
                    let delay = self.weth_delay.unwrap_or(Self::DEFAULT_WETH_DELAY);
                    let retry_delay = Self::WITHDRAW_REVERT_RETRY_DELAY.min(delay);
                    let elapsed_before_retry = delay.saturating_sub(retry_delay);
                    let unlocked_at = self.clock.now().saturating_sub(elapsed_before_retry);
                    warn!(
                        game = %game_address,
                        error = %e,
                        step = "withdraw",
                        retry = "after_backoff",
                        retry_delay_secs = retry_delay.as_secs(),
                        "claimCredit transaction failed, will retry after backoff"
                    );
                    self.tracked.insert(game_address, Some(unlocked_at));
                } else {
                    Self::warn_claim_credit_retry(game_address, &e, "withdraw");
                }
                false
            }
        }
    }

    fn warn_claim_credit_retry(
        game_address: Address,
        error: &ChallengeSubmitError,
        step: &'static str,
    ) {
        warn!(
            game = %game_address,
            error = %error,
            step,
            retry = "immediate",
            "claimCredit transaction failed, will retry"
        );
    }

    fn withdraw_delay_elapsed(
        &self,
        game_address: Address,
        resolved_at: u64,
        unlocked_at: Option<Duration>,
    ) -> bool {
        let delay = self.weth_delay.unwrap_or(Self::DEFAULT_WETH_DELAY);
        let elapsed = unlocked_at.map_or_else(
            || Duration::from_secs(self.clock.wall_clock_unix_secs().saturating_sub(resolved_at)),
            |unlocked_at| self.clock.now().saturating_sub(unlocked_at),
        );

        if elapsed >= delay {
            info!(
                game = %game_address,
                elapsed_secs = elapsed.as_secs(),
                "DelayedWETH delay elapsed, withdrawing bond"
            );
            return true;
        }

        let remaining = delay.saturating_sub(elapsed);
        debug!(
            game = %game_address,
            remaining_secs = remaining.as_secs(),
            "waiting for DelayedWETH delay"
        );
        false
    }

    async fn send_claim_credit<T: TxManager>(
        &self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
        step: &'static str,
    ) -> Result<(), ChallengeSubmitError> {
        ChallengerMetrics::claim_credit_tx_submitted_total().increment(1);
        info!(game = %game_address, step, "submitting claimCredit transaction");
        match submitter
            .send_bond_tx(game_address, game_address, encode_claim_credit_calldata())
            .await
        {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    tx_hash = %tx_hash,
                    step,
                    "claimCredit transaction confirmed"
                );
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
                Ok(())
            }
            Err(e) => {
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin};

    use alloy_primitives::B256;
    use futures::stream::BoxStream;
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockTxManager, addr, empty_factory,
        factory_game, mock_state, receipt_with_status,
    };

    struct FixedClock {
        monotonic: Duration,
        wall_unix: u64,
    }

    impl Clock for FixedClock {
        fn now(&self) -> Duration {
            self.monotonic
        }

        fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::pending())
        }

        fn interval(&self, _period: Duration) -> BoxStream<'static, ()> {
            Box::pin(futures::stream::pending())
        }

        fn wall_clock_unix_secs(&self) -> u64 {
            self.wall_unix
        }
    }

    fn fixed_clock(secs: u64) -> FixedClock {
        FixedClock { monotonic: Duration::from_secs(secs), wall_unix: 2_000_000_000 }
    }

    fn make_manager_with_factory(
        address: Address,
        factory: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
        clock: FixedClock,
    ) -> BondManager<FixedClock> {
        let mut mgr = BondManager::new(
            vec![address],
            "http://localhost:8545".parse().unwrap(),
            factory,
            lookback,
            clock,
        );
        mgr.weth_delay = Some(Duration::from_secs(60));
        mgr
    }

    fn bond_submitter(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (ChallengeSubmitter<MockTxManager>, MockTxManager) {
        let tx_manager = MockTxManager::with_responses(responses);
        (ChallengeSubmitter::new(tx_manager.clone()), tx_manager)
    }

    fn withdraw_retry_case()
    -> (BondManager<FixedClock>, MockAggregateVerifier, Address, Duration, u64) {
        let claim_addr = CLAIM_ADDR;
        let game = addr(0);
        let wall_unix = 2_000_000_000;
        let resolved_at = wall_unix - 3_600;
        let monotonic_secs = 3_700;
        let delay = Duration::from_secs(3_600);
        let clock = FixedClock { monotonic: Duration::from_secs(monotonic_secs), wall_unix };
        let stale_unlocked_at = Duration::from_secs(monotonic_secs - 3_600);

        let mut mgr = make_manager_with_factory(claim_addr, empty_factory(), 1000, clock);
        mgr.weth_delay = Some(delay);
        mgr.track_game(game, claim_addr);
        *mgr.tracked.get_mut(&game).unwrap() = Some(stale_unlocked_at);

        let mut state = mock_state(GameStatus::DefenderWins, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.resolved_at = resolved_at;
        state.bond_unlocked = true;
        state.bond_claimed = false;
        let verifier = MockAggregateVerifier::new([(game, state)].into_iter().collect());

        (mgr, verifier, game, delay, monotonic_secs)
    }

    #[rstest]
    #[case::elapsed(Duration::from_secs(60), Duration::from_secs(900), true)]
    #[case::not_elapsed(Duration::from_secs(3_600), Duration::from_secs(999), false)]
    #[tokio::test]
    async fn awaiting_delay_respects_elapsed_time(
        #[case] delay: Duration,
        #[case] unlocked_at: Duration,
        #[case] expect_withdraw: bool,
    ) {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let mut mgr = make_manager_with_factory(addr, empty_factory(), 1000, fixed_clock(1000));
        mgr.weth_delay = Some(delay);
        mgr.tracked.insert(game, Some(unlocked_at));

        let mut state = mock_state(GameStatus::ChallengerWins, Address::ZERO, 100);
        state.bond_recipient = addr;
        state.resolved_at = 1_999_999_000;
        state.bond_unlocked = true;
        let verifier = MockAggregateVerifier::new([(game, state)].into_iter().collect());
        let responses =
            if expect_withdraw { vec![Ok(receipt_with_status(true, B256::ZERO))] } else { vec![] };
        let (submitter, tx_manager) = bond_submitter(responses);

        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert_eq!(result, expect_withdraw);
        assert_eq!(tx_manager.recorded_calls().len(), usize::from(expect_withdraw));
    }

    #[tokio::test]
    async fn reverted_withdraw_backs_off_before_retrying() {
        let (mut mgr, verifier, game, delay, monotonic_secs) = withdraw_retry_case();
        let (submitter, tx_manager) =
            bond_submitter(vec![Ok(receipt_with_status(false, B256::ZERO))]);

        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert!(!result);
        assert_eq!(tx_manager.recorded_calls().len(), 1);
        let expected_unlocked_at = Duration::from_secs(monotonic_secs).saturating_sub(
            delay.saturating_sub(BondManager::<FixedClock>::WITHDRAW_REVERT_RETRY_DELAY),
        );
        assert!(
            matches!(
                mgr.tracked.get(&game),
                Some(Some(unlocked_at)) if *unlocked_at == expected_unlocked_at
            ),
            "withdraw revert should back off without restarting the full delay"
        );
        assert_ne!(
            expected_unlocked_at,
            Duration::from_secs(monotonic_secs),
            "withdraw revert must not model a fresh DelayedWETH unlock"
        );

        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert!(!result);
        assert_eq!(
            tx_manager.recorded_calls().len(),
            1,
            "next poll should wait instead of submitting again"
        );
    }

    #[tokio::test]
    async fn non_revert_withdraw_failure_does_not_back_off() {
        let (mut mgr, verifier, game, _, _) = withdraw_retry_case();
        let (submitter, tx_manager) = bond_submitter(vec![
            Err(base_tx_manager::TxManagerError::NonceTooLow),
            Err(base_tx_manager::TxManagerError::NonceTooLow),
        ]);

        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert!(!result);
        assert_eq!(tx_manager.recorded_calls().len(), 1);

        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert!(!result);
        assert_eq!(
            tx_manager.recorded_calls().len(),
            2,
            "non-revert failures must not introduce a back-off delay before retry"
        );
    }

    const CLAIM_ADDR: Address = Address::repeat_byte(0xCC);

    fn discover_case(
        game_count: u64,
        bond_recipient: Address,
        zk_prover: Address,
        lookback: u64,
    ) -> (BondManager<FixedClock>, MockAggregateVerifier) {
        let games: Vec<_> = (0..game_count).map(|i| factory_game(i, 0)).collect();
        let verifier_games = (0..game_count)
            .map(|i| {
                let mut state = mock_state(GameStatus::InProgress, zk_prover, 100 + i);
                state.bond_recipient = bond_recipient;
                (addr(i), state)
            })
            .collect();
        let factory = Arc::new(MockDisputeGameFactory::new(games));
        let verifier = MockAggregateVerifier::new(verifier_games);
        (make_manager_with_factory(CLAIM_ADDR, factory, lookback, fixed_clock(0)), verifier)
    }

    #[rstest]
    #[case::by_recipient(3, CLAIM_ADDR, Address::ZERO, 1_000, 3)]
    #[case::by_zk_prover(2, Address::repeat_byte(0xDD), CLAIM_ADDR, 1_000, 2)]
    #[case::capped_to_lookback(1_200, CLAIM_ADDR, Address::ZERO, 500, 500)]
    #[case::within_lookback(500, CLAIM_ADDR, Address::ZERO, 1_000, 500)]
    #[tokio::test]
    async fn discover_tracks_expected_games(
        #[case] game_count: u64,
        #[case] bond_recipient: Address,
        #[case] zk_prover: Address,
        #[case] lookback: u64,
        #[case] expected: usize,
    ) {
        let (mut mgr, verifier) = discover_case(game_count, bond_recipient, zk_prover, lookback);

        mgr.discover_claimable_games(&verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), expected);
    }

    #[tokio::test]
    async fn discover_skips_already_tracked_games() {
        let claim_addr = CLAIM_ADDR;
        let (mut mgr, verifier) = discover_case(2, claim_addr, Address::ZERO, 1000);

        mgr.track_game(addr(0), claim_addr);
        assert_eq!(mgr.tracked_count(), 1);

        mgr.discover_claimable_games(&verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 2);
    }

    #[tokio::test]
    async fn unlocked_game_waits_until_resolved_at_delay_elapsed() {
        let claim_addr = CLAIM_ADDR;
        let game = addr(0);

        let mut state = mock_state(GameStatus::ChallengerWins, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.resolved_at = 1_999_999_900;
        state.bond_unlocked = true;

        let verifier = MockAggregateVerifier::new([(game, state)].into_iter().collect());

        let mut mgr =
            make_manager_with_factory(claim_addr, empty_factory(), 1000, fixed_clock(500));
        mgr.weth_delay = Some(Duration::from_secs(3_600));
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![]);
        let result = mgr.advance_game(game, &verifier, &submitter).await.unwrap();
        assert!(!result, "withdraw should wait for the DelayedWETH delay");
        assert!(
            tx_manager.recorded_calls().is_empty(),
            "no transaction should have been submitted"
        );
    }

    #[tokio::test]
    async fn try_unlock_uses_monotonic_timestamp_for_fresh_unlock() {
        let claim_addr = CLAIM_ADDR;
        let game = addr(0);
        let tx_hash = B256::repeat_byte(0xDD);

        let mut mgr =
            make_manager_with_factory(claim_addr, empty_factory(), 1000, fixed_clock(500));
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![Ok(receipt_with_status(true, tx_hash))]);
        mgr.try_unlock(game, &submitter).await;

        let tracked = mgr.tracked.get(&game).expect("game should still be tracked");
        assert!(
            matches!(tracked, Some(unlocked_at) if *unlocked_at == Duration::from_secs(500)),
            "expected monotonic unlock timestamp, got {tracked:?}",
        );
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }
}
