//! Bond lifecycle management for resolving, unlocking, and withdrawing
//! dispute-game credits.

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

use crate::{ChallengeSubmitError, ChallengeSubmitter, ChallengerMetrics, GameScanner};

/// Phase of the bond claim lifecycle for a single tracked game.
#[derive(Debug)]
enum BondPhase {
    /// The game's dispute period is over; needs a `resolve()` call.
    NeedsResolve,
    /// The game has been resolved; needs the first `claimCredit()` call
    /// to trigger `DelayedWETH.unlock()`.
    NeedsUnlock,
    /// The unlock has been submitted; waiting for the `DelayedWETH` delay
    /// to elapse before the second `claimCredit()` call.
    AwaitingDelay {
        /// Monotonic timestamp at which withdrawal should be retried.
        ready_at: Duration,
        /// Whether `ready_at` was computed with the fallback WETH delay.
        using_default_delay: bool,
    },
}

/// Manages bond claiming for dispute games.
pub struct BondManager<C: Clock> {
    tracked: HashMap<Address, BondPhase>,
    claim_addresses: HashSet<Address>,
    weth_delay: Option<Duration>,
    l1_rpc_url: url::Url,
    clock: C,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    bond_scan_head: u64,
    last_full_scan: Duration,
    lookback: u64,
    discovery_interval: Duration,
}

impl<C: Clock> std::fmt::Debug for BondManager<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondManager").finish_non_exhaustive()
    }
}

impl<C: Clock> BondManager<C> {
    /// Conservative fallback when the onchain `DelayedWETH` delay has not
    /// been read yet.
    const DEFAULT_WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// How long to wait before retrying a reverted withdraw attempt.
    const WITHDRAW_REVERT_RETRY_DELAY: Duration = Duration::from_secs(60);

    /// Creates a new bond manager for the given set of claim addresses.
    pub fn new(
        claim_addresses: Vec<Address>,
        l1_rpc_url: url::Url,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
        discovery_interval: Duration,
        clock: C,
    ) -> Self {
        let last_full_scan = clock.now();
        let set: HashSet<Address> = claim_addresses.into_iter().collect();
        info!(count = set.len(), "bond manager initialized with claim addresses");
        Self {
            tracked: HashMap::new(),
            claim_addresses: set,
            weth_delay: None,
            l1_rpc_url,
            clock,
            factory_client,
            bond_scan_head: 0,
            last_full_scan,
            lookback,
            discovery_interval,
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

        info!(
            game = %game_address,
            recipient = %bond_recipient,
            "tracking game for bond claiming"
        );
        entry.insert(BondPhase::NeedsResolve);
        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        true
    }

    async fn evaluate_game_for_bonds(
        &self,
        index: u64,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> Option<(Address, BondPhase)> {
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

        if !self.claim_addresses.contains(&bond_recipient)
            && (zk_prover == Address::ZERO || !self.claim_addresses.contains(&zk_prover))
        {
            return None;
        }

        let phase = match Self::determine_phase(
            verifier_client,
            game_address,
            &self.clock,
            self.weth_delay,
        )
        .await
        {
            Ok(Some(phase)) => phase,
            Ok(None) => return None,
            Err(e) => {
                warn!(
                    game = %game_address,
                    error = %e,
                    "failed to determine bond phase"
                );
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_PHASE_READ,
                )
                .increment(1);
                return None;
            }
        };

        // Pre-resolve games can match only by zkProver; resolved games must
        // have already moved bondRecipient into our claim set.
        if !matches!(phase, BondPhase::NeedsResolve)
            && !self.claim_addresses.contains(&bond_recipient)
        {
            debug!(
                game = %game_address,
                recipient = %bond_recipient,
                "onchain bondRecipient not in claim addresses \
                 for resolved game, skipping"
            );
            return None;
        }

        Some((game_address, phase))
    }

    async fn track_bond_range(
        &mut self,
        range: std::ops::Range<u64>,
        verifier_client: &dyn AggregateVerifierClient,
        scan_type: &'static str,
    ) -> Vec<Address> {
        let results: Vec<_> = {
            let manager = &*self;
            stream::iter(range)
                .map(|i| manager.evaluate_game_for_bonds(i, verifier_client))
                .buffer_unordered(GameScanner::SCAN_CONCURRENCY)
                .filter_map(std::future::ready)
                .collect()
                .await
        };
        let mut tracked_games = Vec::new();

        for (game_address, phase) in results {
            let Entry::Vacant(entry) = self.tracked.entry(game_address) else {
                continue;
            };

            info!(
                game = %game_address,
                phase = ?phase,
                scan_type,
                "tracked claimable game"
            );
            entry.insert(phase);
            tracked_games.push(game_address);
        }

        tracked_games
    }

    /// Scans recent games at startup to recover bond tracking state.
    pub async fn startup_scan(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<()> {
        if self.claim_addresses.is_empty() {
            return Ok(());
        }

        let game_count = self.factory_client.game_count().await?;
        if game_count == 0 {
            info!("no games in factory, skipping bond startup scan");
            return Ok(());
        }

        let start_index = game_count.saturating_sub(self.lookback);
        info!(start = start_index, end = game_count, "scanning recent games for bond recovery");

        self.track_bond_range(start_index..game_count, verifier_client, "startup").await;

        self.bond_scan_head = game_count;
        self.last_full_scan = self.clock.now();

        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        info!(count = self.tracked.len(), "bond startup scan complete");
        Ok(())
    }

    /// Discovers claimable games via incremental and periodic lookback scans.
    pub async fn discover_claimable_games(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<()> {
        if self.claim_addresses.is_empty() {
            warn!("bond manager is disabled, skipping discovery scan");
            return Ok(());
        }

        let game_count = self.factory_client.game_count().await?;
        if game_count == 0 {
            debug!("no games found, skipping bond discovery scan");
            return Ok(());
        }

        let elapsed = self.clock.now().saturating_sub(self.last_full_scan);
        let is_full_rescan = elapsed >= self.discovery_interval;
        if is_full_rescan {
            let new_head = game_count.saturating_sub(self.lookback);
            debug!(
                new_head,
                game_count,
                lookback = self.lookback,
                "performing periodic full bond rescan"
            );
            self.bond_scan_head = new_head;
        }

        let scan_start = self.bond_scan_head;
        if scan_start >= game_count {
            return Ok(());
        }

        let scan_end = game_count.min(scan_start.saturating_add(self.lookback));
        if scan_end < game_count {
            let behind = game_count - scan_end;
            warn!(
                scan_start,
                scan_end,
                game_count,
                max = self.lookback,
                behind,
                "bond scan span exceeds lookback cap, scanning partial range"
            );
        }

        let scan_type = if is_full_rescan { "full" } else { "incremental" };
        debug!(
            scan_type,
            scan_start,
            scan_end,
            effective_span = scan_end - scan_start,
            game_count,
            tracked = self.tracked.len(),
            "bond discovery scan"
        );

        ChallengerMetrics::bond_discovery_scans_total(scan_type).increment(1);

        let discovered =
            self.track_bond_range(scan_start..scan_end, verifier_client, scan_type).await.len()
                as u64;

        self.bond_scan_head = scan_end;

        if is_full_rescan {
            self.last_full_scan = self.clock.now();
        }

        if discovered > 0 {
            ChallengerMetrics::bond_discovery_games_found_total().increment(discovered);
            ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
            info!(discovered, tracked = self.tracked.len(), scan_type, "bond discovery complete");
        }

        Ok(())
    }

    /// Polls all tracked games and advances each through the bond lifecycle.
    pub async fn poll<T: TxManager>(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) {
        if self.tracked.is_empty() {
            return;
        }

        let tracked = std::mem::take(&mut self.tracked);
        let tracked_count = tracked.len();

        let mut tried_weth_delay = false;
        for (game_address, phase) in tracked {
            if let Some(next_phase) = self
                .advance_game(
                    game_address,
                    phase,
                    verifier_client,
                    submitter,
                    &mut tried_weth_delay,
                )
                .await
            {
                self.tracked.insert(game_address, next_phase);
            }
        }

        if self.tracked.len() != tracked_count {
            ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        }
    }

    async fn advance_game<T: TxManager>(
        &mut self,
        game_address: Address,
        phase: BondPhase,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
        tried_weth_delay: &mut bool,
    ) -> Option<BondPhase> {
        let (result, retry_phase) = match phase {
            BondPhase::NeedsResolve => (
                self.try_resolve(game_address, verifier_client, submitter).await,
                BondPhase::NeedsResolve,
            ),
            BondPhase::NeedsUnlock => (
                self.try_unlock(game_address, verifier_client, submitter, tried_weth_delay).await,
                BondPhase::NeedsUnlock,
            ),
            BondPhase::AwaitingDelay { mut ready_at, using_default_delay } => {
                if using_default_delay {
                    self.ensure_weth_delay(verifier_client, game_address, tried_weth_delay).await;
                    if let Some(delay) = self.weth_delay {
                        ready_at = Self::recompute_default_ready_at(ready_at, delay);
                    }
                }

                let now = self.clock.now();
                if now < ready_at {
                    let remaining = ready_at.saturating_sub(now);
                    debug!(
                        game = %game_address,
                        remaining_secs = remaining.as_secs(),
                        "waiting for DelayedWETH delay"
                    );
                    return Some(BondPhase::AwaitingDelay {
                        ready_at,
                        using_default_delay: using_default_delay && self.weth_delay.is_none(),
                    });
                }

                info!(
                    game = %game_address,
                    ready_at_secs = ready_at.as_secs(),
                    "DelayedWETH delay elapsed, submitting withdraw"
                );
                (
                    self.try_withdraw(
                        game_address,
                        verifier_client,
                        submitter,
                        using_default_delay,
                    )
                    .await,
                    BondPhase::AwaitingDelay {
                        ready_at: now,
                        using_default_delay: using_default_delay && self.weth_delay.is_none(),
                    },
                )
            }
        };

        result.unwrap_or_else(|e| {
            warn!(
                game = %game_address,
                error = %e,
                "failed to advance bond lifecycle"
            );
            Some(retry_phase)
        })
    }

    async fn try_resolve<T: TxManager>(
        &self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<Option<BondPhase>> {
        let status = verifier_client.status(game_address).await?;

        if status == GameStatus::InProgress {
            let game_over = verifier_client.game_over(game_address).await?;
            if !game_over {
                debug!(game = %game_address, "game dispute period not yet elapsed");
                return Ok(Some(BondPhase::NeedsResolve));
            }

            let calldata = encode_resolve_calldata();
            info!(game = %game_address, "submitting resolve transaction");
            match submitter.send_bond_tx(game_address, game_address, calldata).await {
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
                    return Ok(Some(BondPhase::NeedsResolve));
                }
            }
        } else {
            ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ALREADY_RESOLVED)
                .increment(1);
            info!(game = %game_address, status = ?status, "game already resolved");
        }

        let bond_recipient = verifier_client.bond_recipient(game_address).await?;
        if !self.claim_addresses.contains(&bond_recipient) {
            info!(
                game = %game_address,
                recipient = %bond_recipient,
                "bond recipient not in claim addresses after resolve, removing from tracking"
            );
            ChallengerMetrics::bonds_not_claimable_total().increment(1);
            return Ok(None);
        }

        Ok(Some(BondPhase::NeedsUnlock))
    }

    async fn try_unlock<T: TxManager>(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
        tried_weth_delay: &mut bool,
    ) -> eyre::Result<Option<BondPhase>> {
        let (unlocked, resolved_at) = futures::try_join!(
            verifier_client.bond_unlocked(game_address),
            verifier_client.resolved_at(game_address),
        )?;
        if unlocked {
            self.ensure_weth_delay(verifier_client, game_address, tried_weth_delay).await;
            let using_default_delay = self.weth_delay.is_none();
            let delay = self.effective_weth_delay(game_address);
            let phase = Self::unlocked_phase(&self.clock, resolved_at, delay, using_default_delay);
            info!(game = %game_address, resolved_at, phase = ?phase, "bond already unlocked");
            return Ok(Some(phase));
        }

        self.ensure_weth_delay(verifier_client, game_address, tried_weth_delay).await;

        match Self::send_claim_credit(game_address, "unlock", submitter).await {
            Ok(_) => {
                let using_default_delay = self.weth_delay.is_none();
                let delay = self.effective_weth_delay(game_address);
                Ok(Some(BondPhase::AwaitingDelay {
                    ready_at: self.clock.now().saturating_add(delay),
                    using_default_delay,
                }))
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    error = %e,
                    step = "unlock",
                    retry = "immediate",
                    "claimCredit transaction failed, will retry"
                );
                Ok(Some(BondPhase::NeedsUnlock))
            }
        }
    }

    async fn try_withdraw<T: TxManager>(
        &self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
        using_default_delay: bool,
    ) -> eyre::Result<Option<BondPhase>> {
        let claimed = verifier_client.bond_claimed(game_address).await?;
        if claimed {
            info!(game = %game_address, "bond already claimed");
            ChallengerMetrics::bonds_completed_total().increment(1);
            return Ok(None);
        }

        match Self::send_claim_credit(game_address, "withdraw", submitter).await {
            Ok(_) => {
                ChallengerMetrics::bonds_completed_total().increment(1);
                Ok(None)
            }
            Err(e) => {
                let retry_delay = if matches!(&e, crate::ChallengeSubmitError::TxReverted { .. }) {
                    Self::WITHDRAW_REVERT_RETRY_DELAY
                } else {
                    Duration::ZERO
                };
                warn!(
                    game = %game_address,
                    error = %e,
                    step = "withdraw",
                    retry_delay_secs = retry_delay.as_secs(),
                    "claimCredit transaction failed, will retry"
                );
                Ok(Some(BondPhase::AwaitingDelay {
                    ready_at: self.clock.now().saturating_add(retry_delay),
                    using_default_delay: using_default_delay && self.weth_delay.is_none(),
                }))
            }
        }
    }

    fn unlocked_phase(
        clock: &C,
        resolved_at: u64,
        delay: Duration,
        using_default_delay: bool,
    ) -> BondPhase {
        let elapsed_since_resolve =
            Duration::from_secs(clock.wall_clock_unix_secs().saturating_sub(resolved_at));
        BondPhase::AwaitingDelay {
            ready_at: clock.now().saturating_add(delay.saturating_sub(elapsed_since_resolve)),
            using_default_delay,
        }
    }

    fn recompute_default_ready_at(ready_at: Duration, delay: Duration) -> Duration {
        if delay >= Self::DEFAULT_WETH_DELAY {
            ready_at.saturating_add(delay - Self::DEFAULT_WETH_DELAY)
        } else {
            ready_at.saturating_sub(Self::DEFAULT_WETH_DELAY - delay)
        }
    }

    fn effective_weth_delay(&self, game_address: Address) -> Duration {
        self.weth_delay.unwrap_or_else(|| {
            debug!(game = %game_address, "WETH delay not yet known, using default delay");
            Self::DEFAULT_WETH_DELAY
        })
    }

    async fn send_claim_credit<T: TxManager>(
        game_address: Address,
        step: &'static str,
        submitter: &ChallengeSubmitter<T>,
    ) -> Result<(), ChallengeSubmitError> {
        let calldata = encode_claim_credit_calldata();
        ChallengerMetrics::claim_credit_tx_submitted_total().increment(1);
        info!(game = %game_address, step, "submitting claimCredit transaction");

        let result = submitter.send_bond_tx(game_address, game_address, calldata).await;
        match &result {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    tx_hash = %tx_hash,
                    step,
                    "claimCredit transaction confirmed"
                );
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
            }
            Err(_) => {
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1)
            }
        }
        result.map(|_| ())
    }

    async fn determine_phase(
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
        clock: &C,
        weth_delay: Option<Duration>,
    ) -> eyre::Result<Option<BondPhase>> {
        let (bond_claimed, resolved_at, bond_unlocked) = futures::try_join!(
            verifier_client.bond_claimed(game_address),
            verifier_client.resolved_at(game_address),
            verifier_client.bond_unlocked(game_address),
        )?;
        if bond_claimed {
            return Ok(None);
        }
        if bond_unlocked {
            let Some(delay) = weth_delay else {
                return Ok(Some(BondPhase::NeedsUnlock));
            };

            return Ok(Some(Self::unlocked_phase(clock, resolved_at, delay, false)));
        }

        if resolved_at > 0 {
            return Ok(Some(BondPhase::NeedsUnlock));
        }

        Ok(Some(BondPhase::NeedsResolve))
    }

    async fn ensure_weth_delay(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
        tried_weth_delay: &mut bool,
    ) {
        if self.weth_delay.is_some() || *tried_weth_delay {
            return;
        }
        *tried_weth_delay = true;

        let result: eyre::Result<Duration> = async {
            let weth_address = verifier_client.delayed_weth(game_address).await?;
            let weth_client =
                DelayedWETHContractClient::new(weth_address, self.l1_rpc_url.clone())?;
            Ok(weth_client.delay().await?)
        }
        .await;

        match result {
            Ok(delay) => {
                info!(delay_secs = delay.as_secs(), "DelayedWETH delay configured");
                self.weth_delay = Some(delay);
            }
            Err(e) => warn!(error = %e, "failed to read DelayedWETH delay, will retry later"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        future::Future,
        io::{ErrorKind, Read, Write},
        net::TcpListener,
        pin::Pin,
        sync::Arc,
        thread,
    };

    use alloy_primitives::B256;
    use futures::stream::BoxStream;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, SharedMockTxManager,
        TEST_DISCOVERY_INTERVAL, addr, empty_factory, factory_game, mock_state,
        receipt_with_status,
    };

    const TEST_WETH_DELAY: Duration = Duration::from_secs(60);
    const WITHDRAW_READY_MONOTONIC_SECS: u64 = 3_700;

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

    fn claim_addr() -> Address {
        Address::repeat_byte(0xCC)
    }

    fn game_state(
        status: GameStatus,
        bond_recipient: Address,
        resolved_at: u64,
        bond_unlocked: bool,
    ) -> crate::test_utils::MockGameState {
        let mut state = mock_state(status, Address::ZERO, 100);
        state.bond_recipient = bond_recipient;
        state.resolved_at = resolved_at;
        state.bond_unlocked = bond_unlocked;
        state
    }

    fn verifier(state: crate::test_utils::MockGameState) -> Arc<MockAggregateVerifier> {
        Arc::new(MockAggregateVerifier::new(HashMap::from([(addr(0), state)])))
    }

    fn ready_phase() -> BondPhase {
        BondPhase::AwaitingDelay {
            ready_at: Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS),
            using_default_delay: false,
        }
    }

    async fn advance(
        mgr: &mut BondManager<FixedClock>,
        phase: BondPhase,
        verifier: &Arc<MockAggregateVerifier>,
        submitter: &ChallengeSubmitter<SharedMockTxManager>,
    ) -> BondPhase {
        let mut tried_weth_delay = false;
        mgr.advance_game(addr(0), phase, &**verifier, submitter, &mut tried_weth_delay)
            .await
            .unwrap()
    }

    fn bond_submitter(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (ChallengeSubmitter<SharedMockTxManager>, SharedMockTxManager) {
        let tx_manager = SharedMockTxManager::with_responses(responses);
        (ChallengeSubmitter::new(tx_manager.clone()), tx_manager)
    }

    fn rpc_id(request: &str) -> &str {
        let Some((_, tail)) = request.split_once("\"id\"") else {
            return "0";
        };
        let Some((_, value)) = tail.split_once(':') else {
            return "0";
        };
        value
            .trim_start()
            .split([',', '}'])
            .next()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("0")
    }

    fn content_length(request: &str) -> Option<usize> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
    }

    fn delayed_weth_rpc(delay: Option<Duration>) -> (url::Url, thread::JoinHandle<()>) {
        delayed_weth_rpc_sequence(vec![delay])
    }

    fn delayed_weth_rpc_sequence(
        delays: Vec<Option<Duration>>,
    ) -> (url::Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let handle = thread::spawn(move || {
            for delay in delays {
                let mut stream = (0..100)
                    .find_map(|_| match listener.accept() {
                        Ok((stream, _)) => Some(stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                            None
                        }
                        Err(e) => panic!("accept failed: {e}"),
                    })
                    .expect("timed out waiting for DelayedWETH delay request");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    if body.len() >= content_length(headers).unwrap_or(0) {
                        break;
                    }
                }

                let request = String::from_utf8_lossy(&request);
                let body = delay.map_or_else(
                    || {
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32000,"message":"boom"}}}}"#,
                            rpc_id(&request)
                        )
                    },
                    |delay| {
                        let result = format!("0x{:064x}", delay.as_secs());
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{},"result":"{}"}}"#,
                            rpc_id(&request),
                            result
                        )
                    },
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (url, handle)
    }

    fn manager(
        claim_addr: Address,
        factory: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
        clock: FixedClock,
    ) -> BondManager<FixedClock> {
        let mut mgr = BondManager::new(
            vec![claim_addr],
            "http://localhost:8545".parse().unwrap(),
            factory,
            lookback,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.weth_delay = Some(TEST_WETH_DELAY);
        mgr
    }

    fn withdraw_ready_fixture(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (
        BondManager<FixedClock>,
        Arc<MockAggregateVerifier>,
        ChallengeSubmitter<SharedMockTxManager>,
        SharedMockTxManager,
    ) {
        let claim_addr = claim_addr();
        let wall_unix = 2_000_000_000;
        let delay = Duration::from_secs(3_600);
        let clock =
            FixedClock { monotonic: Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS), wall_unix };

        let mut mgr = manager(claim_addr, empty_factory(), 1000, clock);
        mgr.weth_delay = Some(delay);

        let state = game_state(GameStatus::DefenderWins, claim_addr, wall_unix - 3_600, true);
        let verifier = verifier(state);
        let (submitter, tx_manager) = bond_submitter(responses);

        (mgr, verifier, submitter, tx_manager)
    }

    #[test]
    fn track_game_filters_by_claim_address() {
        let claim_addr = Address::repeat_byte(0x01);
        let other = Address::repeat_byte(0x02);
        let game = Address::repeat_byte(0xAA);

        let mut mgr = manager(claim_addr, empty_factory(), 1000, fixed_clock(0));
        assert!(mgr.track_game(game, claim_addr));
        assert!(!mgr.track_game(game, claim_addr));
        assert!(!mgr.track_game(Address::repeat_byte(0xBB), other));
    }

    #[tokio::test]
    async fn already_resolved_game_advances_to_unlock() {
        let claim_addr = Address::repeat_byte(0x01);
        let mut mgr = manager(claim_addr, empty_factory(), 1000, fixed_clock(0));

        let verifier = verifier(game_state(GameStatus::DefenderWins, claim_addr, 100, false));
        let (submitter, tx_manager) = bond_submitter(vec![]);

        let result = advance(&mut mgr, BondPhase::NeedsResolve, &verifier, &submitter).await;

        assert!(matches!(result, BondPhase::NeedsUnlock));
        assert!(tx_manager.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn reverted_withdraw_backs_off_before_retrying() {
        let (mut mgr, verifier, submitter, tx_manager) =
            withdraw_ready_fixture(vec![Ok(receipt_with_status(false, B256::ZERO))]);

        let phase = advance(&mut mgr, ready_phase(), &verifier, &submitter).await;
        assert_eq!(tx_manager.recorded_calls().len(), 1);
        let expected_ready_at = Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS)
            .saturating_add(BondManager::<FixedClock>::WITHDRAW_REVERT_RETRY_DELAY);
        assert!(
            matches!(
                phase,
                BondPhase::AwaitingDelay { ready_at, .. } if ready_at == expected_ready_at
            ),
            "withdraw revert should back off briefly"
        );

        let phase = advance(&mut mgr, phase, &verifier, &submitter).await;
        assert!(matches!(phase, BondPhase::AwaitingDelay { .. }));
        assert_eq!(
            tx_manager.recorded_calls().len(),
            1,
            "next poll should wait in AwaitingDelay instead of submitting again"
        );
    }

    #[tokio::test]
    async fn non_revert_withdraw_failure_does_not_back_off() {
        let (mut mgr, verifier, submitter, tx_manager) = withdraw_ready_fixture(vec![
            Err(base_tx_manager::TxManagerError::NonceTooLow),
            Err(base_tx_manager::TxManagerError::NonceTooLow),
        ]);

        let phase = advance(&mut mgr, ready_phase(), &verifier, &submitter).await;
        assert_eq!(tx_manager.recorded_calls().len(), 1);
        assert!(
            matches!(
                phase,
                BondPhase::AwaitingDelay { ready_at, .. }
                    if ready_at == Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS)
            ),
            "non-revert withdraw failure must retry immediately"
        );

        let phase = advance(&mut mgr, phase, &verifier, &submitter).await;
        assert_eq!(
            tx_manager.recorded_calls().len(),
            2,
            "non-revert failures must not introduce a back-off delay before retry"
        );
        assert!(
            matches!(
                phase,
                BondPhase::AwaitingDelay { ready_at, .. }
                    if ready_at == Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS)
            ),
            "non-revert withdraw failure must stay ready for retry"
        );
    }

    fn discovery_mocks(
        game_count: u64,
        bond_recipient: Address,
        zk_prover: Address,
    ) -> (Arc<dyn DisputeGameFactoryClient>, Arc<MockAggregateVerifier>) {
        let games: Vec<_> = (0..game_count).map(|i| factory_game(i, 0)).collect();
        let verifier_games = (0..game_count)
            .map(|i| {
                let mut state = mock_state(GameStatus::InProgress, zk_prover, 100 + i);
                state.bond_recipient = bond_recipient;
                (addr(i), state)
            })
            .collect();
        let factory: Arc<dyn DisputeGameFactoryClient> =
            Arc::new(MockDisputeGameFactory::new(games));
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        (factory, verifier)
    }

    async fn assert_discovery(
        game_count: u64,
        scan_head: u64,
        lookback: u64,
        ticks: usize,
        expected_head: u64,
        expected_tracked: usize,
    ) {
        let claim_addr = claim_addr();
        let (factory, verifier) = discovery_mocks(game_count, claim_addr, Address::ZERO);
        let mut mgr = manager(claim_addr, factory, lookback, fixed_clock(0));
        mgr.bond_scan_head = scan_head;

        for _ in 0..ticks {
            mgr.discover_claimable_games(&*verifier).await.unwrap();
        }

        assert_eq!(mgr.bond_scan_head, expected_head);
        assert_eq!(mgr.tracked_count(), expected_tracked);
    }

    #[tokio::test]
    async fn discover_filters_games() {
        for (game_count, bond_recipient, zk_prover, expected_tracked) in [
            (3_u64, claim_addr(), Address::ZERO, 3_usize),
            (2, Address::repeat_byte(0xDD), claim_addr(), 2),
            (3, Address::repeat_byte(0xDD), Address::ZERO, 0),
        ] {
            let claim_addr = claim_addr();
            let (factory, verifier) = discovery_mocks(game_count, bond_recipient, zk_prover);

            let mut mgr = manager(claim_addr, factory, 1000, fixed_clock(0));

            mgr.discover_claimable_games(&*verifier).await.unwrap();
            assert_eq!(mgr.tracked_count(), expected_tracked);
            assert_eq!(mgr.bond_scan_head, game_count);
        }
    }

    #[tokio::test]
    async fn discover_skips_already_tracked_games() {
        let claim_addr = claim_addr();
        let (factory, verifier) = discovery_mocks(2, claim_addr, Address::ZERO);

        let mut mgr = manager(claim_addr, factory, 1000, fixed_clock(0));

        mgr.track_game(addr(0), claim_addr);
        assert_eq!(mgr.tracked_count(), 1);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 2);
    }

    #[tokio::test]
    async fn discover_skips_already_claimed_games() {
        let claim_addr = claim_addr();

        let games = vec![factory_game(0, 0)];
        let mut state = game_state(GameStatus::ChallengerWins, claim_addr, 500, false);
        state.bond_claimed = true;

        let factory: Arc<dyn DisputeGameFactoryClient> =
            Arc::new(MockDisputeGameFactory::new(games));
        let verifier = verifier(state);

        let mut mgr = manager(claim_addr, factory, 1000, fixed_clock(0));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0, "claimed game should not be tracked");
    }

    #[tokio::test]
    async fn discover_updates_watermark() {
        for (scan_head, expected_head, expected_tracked) in [(3_u64, 5_u64, 2_usize), (5, 5, 0)] {
            assert_discovery(5, scan_head, 1000, 1, expected_head, expected_tracked).await;
        }
    }

    #[tokio::test]
    async fn discover_full_rescan_resets_watermark() {
        let claim_addr = claim_addr();
        let (factory, verifier) = discovery_mocks(10, claim_addr, Address::ZERO);

        let clock = fixed_clock(1000);
        let mut mgr = manager(claim_addr, factory, 5, clock);

        mgr.bond_scan_head = 10;

        mgr.last_full_scan = Duration::from_secs(1000).saturating_sub(TEST_DISCOVERY_INTERVAL);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.bond_scan_head, 10);
        assert_eq!(mgr.tracked_count(), 5);
    }

    #[tokio::test]
    async fn discover_disabled_when_no_claim_addresses() {
        let (_, verifier) = discovery_mocks(5, Address::repeat_byte(0xCC), Address::ZERO);

        let mut mgr = BondManager::new(
            vec![],
            "http://localhost:8545".parse().unwrap(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(0),
        );

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
    }

    #[tokio::test]
    async fn determine_phase_returns_unlocked_phase() {
        for (monotonic_secs, delay, expected_ready_at) in [
            (500_u64, Duration::from_secs(120), Duration::from_secs(520)),
            (10, Duration::from_secs(60), Duration::from_secs(10)),
        ] {
            let game = addr(0);
            let verifier = verifier(game_state(
                GameStatus::ChallengerWins,
                Address::ZERO,
                1_999_999_900,
                true,
            ));

            let clock = fixed_clock(monotonic_secs);
            let phase = BondManager::determine_phase(&*verifier, game, &clock, Some(delay))
                .await
                .unwrap()
                .unwrap();
            match (expected_ready_at, phase) {
                (expected, BondPhase::AwaitingDelay { ready_at, .. }) if ready_at == expected => {}
                (_, phase) => panic!("unexpected phase: {phase:?}"),
            }
        }
    }

    #[tokio::test]
    async fn try_unlock_advances_to_withdraw_when_already_unlocked_and_delay_elapsed() {
        let claim_addr = claim_addr();
        let game = addr(0);
        let verifier =
            verifier(game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_800, true));
        let mut mgr = manager(claim_addr, empty_factory(), 1000, fixed_clock(500));
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![]);
        let mut tried_weth_delay = false;
        let result =
            mgr.try_unlock(game, &*verifier, &submitter, &mut tried_weth_delay).await.unwrap();
        assert!(
            matches!(
                result,
                Some(BondPhase::AwaitingDelay { ready_at, .. })
                    if ready_at == Duration::from_secs(500)
            ),
            "expected ready AwaitingDelay, got {result:?}",
        );
        assert!(
            tx_manager.recorded_calls().is_empty(),
            "no transaction should have been submitted"
        );
    }

    #[tokio::test]
    async fn try_unlock_uses_monotonic_timestamp_for_fresh_unlock() {
        let claim_addr = claim_addr();
        let game = addr(0);
        let tx_hash = B256::repeat_byte(0xDD);
        let verifier =
            verifier(game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_900, false));
        let mut mgr = manager(claim_addr, empty_factory(), 1000, fixed_clock(500));
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![Ok(receipt_with_status(true, tx_hash))]);
        let mut tried_weth_delay = false;
        let result =
            mgr.try_unlock(game, &*verifier, &submitter, &mut tried_weth_delay).await.unwrap();
        assert!(
            matches!(
                result,
                Some(BondPhase::AwaitingDelay { ready_at, .. })
                    if ready_at == Duration::from_secs(560)
            ),
            "expected AwaitingDelay with monotonic deadline, got {result:?}",
        );
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn try_unlock_loads_weth_delay_before_unlock() {
        let claim_addr = claim_addr();
        let game = addr(0);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_000, false);
        state.delayed_weth = addr(9);
        let verifier = verifier(state);
        let (rpc_url, handle) = delayed_weth_rpc(Some(TEST_WETH_DELAY));
        let mut mgr = BondManager::new(
            vec![claim_addr],
            rpc_url,
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(500),
        );
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![Ok(receipt_with_status(true, tx_hash))]);
        let mut tried_weth_delay = false;
        let result =
            mgr.try_unlock(game, &*verifier, &submitter, &mut tried_weth_delay).await.unwrap();
        handle.join().unwrap();

        assert!(
            matches!(
                result,
                Some(BondPhase::AwaitingDelay { ready_at, .. })
                    if ready_at == Duration::from_secs(560)
            ),
            "expected AwaitingDelay from unlock time, got {result:?}",
        );
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn try_unlock_submits_with_default_when_weth_delay_unavailable() {
        let claim_addr = claim_addr();
        let game = addr(0);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_000, false);
        state.delayed_weth = addr(9);
        let verifier = verifier(state);
        let (rpc_url, handle) = delayed_weth_rpc(None);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            rpc_url,
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(500),
        );
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![Ok(receipt_with_status(true, tx_hash))]);
        let mut tried_weth_delay = false;
        let result =
            mgr.try_unlock(game, &*verifier, &submitter, &mut tried_weth_delay).await.unwrap();
        handle.join().unwrap();

        assert!(
            matches!(
                result,
                Some(BondPhase::AwaitingDelay { ready_at, .. })
                    if ready_at
                        == Duration::from_secs(500)
                            .saturating_add(BondManager::<FixedClock>::DEFAULT_WETH_DELAY)
            ),
            "expected fallback AwaitingDelay from unlock time, got {result:?}",
        );
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn poll_tries_weth_delay_once_per_tick() {
        let claim_addr = claim_addr();
        let tx_hash = B256::repeat_byte(0xDD);
        let states = (0..2).map(|i| {
            let mut state =
                game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_000, false);
            state.delayed_weth = addr(9);
            (addr(i), state)
        });
        let verifier = Arc::new(MockAggregateVerifier::new(states.collect()));
        let mut mgr = BondManager::new(
            vec![claim_addr],
            "http://127.0.0.1:0".parse().unwrap(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(500),
        );
        mgr.tracked.insert(addr(0), BondPhase::NeedsUnlock);
        mgr.tracked.insert(addr(1), BondPhase::NeedsUnlock);

        let (submitter, tx_manager) = bond_submitter(vec![
            Ok(receipt_with_status(true, tx_hash)),
            Ok(receipt_with_status(true, tx_hash)),
        ]);
        mgr.poll(&*verifier, &submitter).await;

        assert_eq!(verifier.delayed_weth_reads.lock().unwrap().len(), 1);
        assert_eq!(tx_manager.recorded_calls().len(), 2);
    }

    #[tokio::test]
    async fn awaiting_delay_recomputes_default_when_weth_delay_recovers() {
        let claim_addr = claim_addr();
        let game = addr(0);
        let tx_hash = B256::repeat_byte(0xDD);
        let mut state = game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_000, false);
        state.delayed_weth = addr(9);
        let verifier = verifier(state);
        let (rpc_url, handle) = delayed_weth_rpc_sequence(vec![None, Some(TEST_WETH_DELAY)]);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            rpc_url,
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(500),
        );
        mgr.track_game(game, claim_addr);

        let (submitter, tx_manager) = bond_submitter(vec![
            Ok(receipt_with_status(true, tx_hash)),
            Ok(receipt_with_status(true, tx_hash)),
        ]);
        let mut tried_weth_delay = false;
        let phase = mgr
            .try_unlock(game, &*verifier, &submitter, &mut tried_weth_delay)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&phase, BondPhase::AwaitingDelay { using_default_delay: true, .. }),
            "expected default-delay AwaitingDelay, got {phase:?}",
        );

        mgr.clock.monotonic = Duration::from_secs(561);
        let mut tried_weth_delay = false;
        let result =
            mgr.advance_game(game, phase, &*verifier, &submitter, &mut tried_weth_delay).await;
        handle.join().unwrap();

        assert!(result.is_none(), "withdraw should complete after recovered delay");
        assert_eq!(tx_manager.recorded_calls().len(), 2);
    }

    #[tokio::test]
    async fn reverted_default_withdraw_keeps_learning_weth_delay() {
        let claim_addr = claim_addr();
        let long_delay =
            BondManager::<FixedClock>::DEFAULT_WETH_DELAY.saturating_add(Duration::from_secs(3600));
        let mut state = game_state(GameStatus::ChallengerWins, claim_addr, 1_999_999_000, true);
        state.delayed_weth = addr(9);
        let verifier = verifier(state);
        let (rpc_url, handle) = delayed_weth_rpc_sequence(vec![None, Some(long_delay)]);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            rpc_url,
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(WITHDRAW_READY_MONOTONIC_SECS),
        );
        let (submitter, tx_manager) =
            bond_submitter(vec![Ok(receipt_with_status(false, B256::ZERO))]);
        let phase = BondPhase::AwaitingDelay {
            ready_at: Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS),
            using_default_delay: true,
        };

        let phase = advance(&mut mgr, phase, &verifier, &submitter).await;
        assert!(
            matches!(&phase, BondPhase::AwaitingDelay { using_default_delay: true, .. }),
            "reverted fallback withdraw should keep retrying delay lookup, got {phase:?}",
        );

        mgr.clock.monotonic = Duration::from_secs(WITHDRAW_READY_MONOTONIC_SECS)
            .saturating_add(BondManager::<FixedClock>::WITHDRAW_REVERT_RETRY_DELAY);
        let phase = advance(&mut mgr, phase, &verifier, &submitter).await;
        handle.join().unwrap();

        assert!(
            matches!(phase, BondPhase::AwaitingDelay { using_default_delay: false, .. }),
            "recovered WETH delay should replace fallback state"
        );
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn discover_handles_empty_factory() {
        let claim_addr = claim_addr();

        let mut mgr = BondManager::new(
            vec![claim_addr],
            "http://localhost:8545".parse().unwrap(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            fixed_clock(0),
        );

        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        assert_eq!(mgr.bond_scan_head, 0);
    }

    #[tokio::test]
    async fn discover_advances_by_lookback() {
        for (lookback, game_count, ticks, expected_head, expected_tracked) in [
            (500_u64, 1200_u64, 1_usize, 500_u64, 500_usize),
            (500, 800, 2, 800, 800),
            (1000, 500, 1, 500, 500),
        ] {
            assert_discovery(game_count, 0, lookback, ticks, expected_head, expected_tracked).await;
        }
    }
}
