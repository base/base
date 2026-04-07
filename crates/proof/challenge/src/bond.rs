//! Bond lifecycle management.
//!
//! The [`BondManager`] tracks dispute games through a multi-phase credit
//! claim lifecycle:
//!
//! 1. **[`NeedsResolve`](BondPhase::NeedsResolve)** — wait for the game's
//!    dispute period to expire, then call `resolve()`.
//! 2. **[`NeedsUnlock`](BondPhase::NeedsUnlock)** — call `claimCredit()`
//!    to trigger `DelayedWETH.unlock()`.
//! 3. **[`AwaitingDelay`](BondPhase::AwaitingDelay)** — wait for the
//!    `DelayedWETH` delay to elapse.
//! 4. **[`NeedsWithdraw`](BondPhase::NeedsWithdraw)** — call `claimCredit()`
//!    again to complete the withdrawal.
//!
//! A comma-separated list of addresses is provided via the
//! `BASE_CHALLENGER_BOND_CLAIM_ADDRESSES` env var. The manager tracks any
//! game whose onchain `bondRecipient` matches one of those addresses,
//! regardless of the game's resolution outcome (`CHALLENGER_WINS` or
//! `DEFENDER_WINS`). This allows claiming bonds both for games won by the
//! challenger and games proposed by addresses in the claim set.
//!
//! During startup recovery, `zkProver` is also checked against the claim
//! addresses to recover pre-resolve challenged games, since `bondRecipient`
//! is only updated to the challenger's address during `resolve()`. For
//! already-resolved games matched solely via `zkProver`, the onchain
//! `bondRecipient` is re-verified against the claim set before tracking.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, encode_claim_credit_calldata, encode_resolve_calldata,
};
use base_runtime::Clock;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{ChallengerMetrics, GameScanner};

/// Reason a game was removed from tracking after [`BondManager::advance_game`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalReason {
    /// Bond was successfully claimed — the full lifecycle completed.
    Completed,
    /// Bond is not claimable by us (recipient changed after resolve).
    NotClaimable,
}

/// Phase of the bond claim lifecycle for a single tracked game.
#[derive(Debug, Clone)]
pub enum BondPhase {
    /// The game's dispute period is over; needs a `resolve()` call.
    NeedsResolve,
    /// The game has been resolved; needs the first `claimCredit()` call
    /// to trigger `DelayedWETH.unlock()`.
    NeedsUnlock,
    /// The unlock has been submitted; waiting for the `DelayedWETH` delay
    /// to elapse before the second `claimCredit()` call.
    AwaitingDelay {
        /// Monotonic timestamp at which the unlock occurred.
        unlocked_at: Duration,
    },
    /// The delay has elapsed; needs the second `claimCredit()` call to
    /// complete the withdrawal.
    NeedsWithdraw,
    /// Bond fully claimed. The entry will be removed from tracking.
    Completed,
}

/// A game being tracked for bond lifecycle management.
#[derive(Debug, Clone)]
pub struct TrackedGame {
    /// Current lifecycle phase.
    pub phase: BondPhase,
    /// The address that will receive the bond.
    pub bond_recipient: Address,
}

/// Manages the bond claim lifecycle for dispute games.
///
/// After a successful `challenge()` submission, games are registered here.
/// On each [`poll`](Self::poll) tick the manager checks each tracked game's
/// onchain state and submits the next transaction in the lifecycle.
///
/// When bond claim addresses are configured, the manager also continuously
/// discovers claimable games via [`discover_claimable_games`](Self::discover_claimable_games),
/// scanning both newly created games and periodically rescanning the
/// lookback window to catch games challenged or resolved by other actors.
#[derive(derive_more::Debug)]
pub struct BondManager<C: Clock> {
    /// Games being tracked, keyed by proxy address.
    tracked: HashMap<Address, TrackedGame>,
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
    #[debug(skip)]
    clock: C,
    /// Factory client for querying game indices during bond discovery.
    #[debug(skip)]
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    /// Highest game index scanned for bond discovery. Incremental scans
    /// start from this index; periodic full rescans reset it backward.
    bond_scan_head: u64,
    /// Monotonic timestamp of the last full rescan completion.
    last_full_scan: Duration,
    /// Number of games to look back during periodic full rescans.
    lookback: u64,
}

impl<C: Clock> BondManager<C> {
    /// Conservative fallback when the onchain `DelayedWETH` delay has not
    /// been read yet. If the real delay is shorter the withdraw will simply
    /// succeed earlier; if longer, the attempt reverts and is retried.
    const DEFAULT_WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// How often a full rescan of the lookback window is performed to catch
    /// state transitions (games challenged or resolved by other actors).
    const BOND_DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);

    /// Creates a new bond manager for the given set of claim addresses.
    pub fn new(
        claim_addresses: Vec<Address>,
        l1_rpc_url: url::Url,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
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
        }
    }

    /// Returns `true` if bond claiming is enabled (at least one claim address configured).
    pub fn is_enabled(&self) -> bool {
        !self.claim_addresses.is_empty()
    }

    /// Sets the `DelayedWETH` withdrawal delay.
    pub fn set_weth_delay(&mut self, delay: Duration) {
        info!(delay_secs = delay.as_secs(), "DelayedWETH delay configured");
        self.weth_delay = Some(delay);
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

        if self.tracked.contains_key(&game_address) {
            debug!(game = %game_address, "game already tracked for bond claiming");
            return false;
        }

        info!(
            game = %game_address,
            recipient = %bond_recipient,
            "tracking game for bond claiming"
        );
        self.tracked
            .insert(game_address, TrackedGame { phase: BondPhase::NeedsResolve, bond_recipient });
        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        true
    }

    /// Returns `true` if the given game is being tracked.
    pub fn is_tracking(&self, game_address: &Address) -> bool {
        self.tracked.contains_key(game_address)
    }

    /// Updates the phase of a tracked game. No-op if the game is not tracked.
    fn set_phase(&mut self, game_address: Address, phase: BondPhase) {
        if let Some(game) = self.tracked.get_mut(&game_address) {
            game.phase = phase;
        }
    }

    /// Evaluates a single game for bond tracking eligibility.
    ///
    /// Fetches the game's `bondRecipient` and `zkProver`, matches them
    /// against `claim_addresses`, determines the onchain lifecycle phase,
    /// and returns the game address, matched address, and phase if the game
    /// is eligible for tracking. Returns `None` when the game is not
    /// relevant, already claimed, or an RPC error occurs.
    async fn evaluate_game_for_bonds(
        index: u64,
        factory_client: &dyn DisputeGameFactoryClient,
        verifier_client: &dyn AggregateVerifierClient,
        claim_addresses: &HashSet<Address>,
        clock: &C,
    ) -> Option<(Address, Address, Option<BondPhase>)> {
        let game_at = match factory_client.game_at_index(index).await {
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
        let matched_address = if claim_addresses.contains(&bond_recipient) {
            bond_recipient
        } else if zk_prover != Address::ZERO && claim_addresses.contains(&zk_prover) {
            zk_prover
        } else {
            return None;
        };

        let phase = match Self::determine_phase(verifier_client, game_address, clock).await {
            Ok(phase) => phase,
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

        // For already-resolved games, verify the current onchain
        // `bondRecipient` is in our claim addresses. Games matched via
        // `zkProver` may have a `bondRecipient` that is not in our
        // claim set (e.g. a game where our challenge was nullified and
        // the bond goes to the game creator). Pre-resolve games are
        // kept — `bondRecipient` will be re-verified after resolve in
        // `try_resolve`.
        if let Some(ref p) = phase
            && !matches!(p, BondPhase::NeedsResolve)
            && !claim_addresses.contains(&bond_recipient)
        {
            debug!(
                game = %game_address,
                recipient = %bond_recipient,
                "onchain bondRecipient not in claim addresses \
                 for resolved game, skipping"
            );
            return None;
        }

        Some((game_address, matched_address, phase))
    }

    /// Evaluates all games in `range` concurrently for bond tracking
    /// eligibility, returning one entry per evaluated game.
    async fn evaluate_bond_range(
        range: std::ops::Range<u64>,
        factory_client: &Arc<dyn DisputeGameFactoryClient>,
        verifier_client: &dyn AggregateVerifierClient,
        claim_addresses: &HashSet<Address>,
        clock: &C,
    ) -> Vec<Option<(Address, Address, Option<BondPhase>)>> {
        stream::iter(range)
            .map(|i| {
                let fc = &**factory_client;
                async move {
                    Self::evaluate_game_for_bonds(i, fc, verifier_client, claim_addresses, clock)
                        .await
                }
            })
            .buffer_unordered(GameScanner::SCAN_CONCURRENCY)
            .collect()
            .await
    }

    /// Scans recent games at startup to recover bond tracking state after a
    /// restart.
    ///
    /// Iterates the last `lookback` games from the factory concurrently and
    /// checks if any have a `bondRecipient` or `zkProver` matching our claim
    /// addresses. The `zkProver` check is necessary because before
    /// `resolve()`, `bondRecipient` is the game creator — only after
    /// resolution does it update to the challenger's address. Games that are
    /// already fully claimed are skipped.
    ///
    /// Also reads the `DelayedWETH` delay from the first game found, if the
    /// delay has not been set yet. Sets the bond discovery watermark to the
    /// current `game_count` so that subsequent
    /// [`discover_claimable_games`](Self::discover_claimable_games) calls
    /// start scanning from where startup left off.
    pub async fn startup_scan(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let factory_client = Arc::clone(&self.factory_client);
        let game_count = factory_client.game_count().await?;
        if game_count == 0 {
            info!("no games in factory, skipping bond startup scan");
            return Ok(());
        }

        let start_index = game_count.saturating_sub(self.lookback);
        info!(start = start_index, end = game_count, "scanning recent games for bond recovery");

        let results = Self::evaluate_bond_range(
            start_index..game_count,
            &factory_client,
            verifier_client,
            &self.claim_addresses,
            &self.clock,
        )
        .await;

        // Process results sequentially: insert tracked games and resolve the
        // WETH delay from the first relevant game.
        for (game_address, bond_recipient, phase) in results.into_iter().flatten() {
            // Resolve the WETH delay from the first available game,
            // including already-claimed ones, so that the delay is
            // bootstrapped as early as possible.
            if self.weth_delay.is_none()
                && let Err(e) = self.resolve_weth_delay(verifier_client, game_address).await
            {
                warn!(error = %e, "failed to read DelayedWETH delay, will retry later");
            }

            let Some(phase) = phase else {
                continue; // already claimed, skip
            };

            info!(
                game = %game_address,
                recipient = %bond_recipient,
                phase = ?phase,
                "recovered game for bond tracking"
            );
            self.tracked.insert(game_address, TrackedGame { phase, bond_recipient });
        }

        // Set the discovery watermark so continuous scanning starts from
        // where startup left off.
        self.bond_scan_head = game_count;
        self.last_full_scan = self.clock.now();

        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        info!(count = self.tracked.len(), "bond startup scan complete");
        Ok(())
    }

    /// Discovers claimable games via two-tier scanning.
    ///
    /// **Incremental** (every call): scans from `bond_scan_head` to
    /// `game_count`, catching newly created games. Typically zero to a
    /// handful of games per tick, costing a single `game_count()` RPC
    /// when idle.
    ///
    /// **Periodic full rescan** (every [`BOND_DISCOVERY_INTERVAL`](Self::BOND_DISCOVERY_INTERVAL)):
    /// resets the watermark backward by `lookback` to re-evaluate games
    /// whose state may have changed (e.g. challenged or resolved by
    /// another actor since the last scan).
    pub async fn discover_claimable_games(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> eyre::Result<()> {
        if !self.is_enabled() {
            warn!("bond manager is disabled, skipping discovery scan");
            return Ok(());
        }

        let factory_client = Arc::clone(&self.factory_client);
        let game_count = factory_client.game_count().await?;
        if game_count == 0 {
            debug!("no games found, skipping bond discovery scan");
            return Ok(());
        }

        // Periodic full rescan: reset watermark to re-evaluate the
        // lookback window and catch state transitions on older games.
        let elapsed = self.clock.now().saturating_sub(self.last_full_scan);
        let is_full_rescan = elapsed >= Self::BOND_DISCOVERY_INTERVAL;
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

        let span = game_count - scan_start;
        let scan_type = if is_full_rescan { "full" } else { "incremental" };
        debug!(
            scan_type,
            scan_start,
            game_count,
            span,
            tracked = self.tracked.len(),
            "bond discovery scan"
        );

        ChallengerMetrics::bond_discovery_scans_total(scan_type).increment(1);

        let results = Self::evaluate_bond_range(
            scan_start..game_count,
            &factory_client,
            verifier_client,
            &self.claim_addresses,
            &self.clock,
        )
        .await;

        let mut discovered = 0u64;

        for (game_address, bond_recipient, phase) in results.into_iter().flatten() {
            // Skip games already being tracked.
            if self.tracked.contains_key(&game_address) {
                continue;
            }

            let Some(phase) = phase else {
                continue;
            };

            if self.weth_delay.is_none()
                && let Err(e) = self.resolve_weth_delay(verifier_client, game_address).await
            {
                warn!(error = %e, "failed to read DelayedWETH delay, will retry later");
            }

            info!(
                game = %game_address,
                recipient = %bond_recipient,
                phase = ?phase,
                scan_type,
                "discovered claimable game"
            );
            self.tracked.insert(game_address, TrackedGame { phase, bond_recipient });
            discovered += 1;
        }

        // Advance watermark past the scanned range.
        self.bond_scan_head = game_count;

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
    ///
    /// Called once per driver tick. Errors on individual games are logged and
    /// do not abort processing of remaining games.
    pub async fn poll(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) {
        if self.tracked.is_empty() {
            return;
        }

        // Lazily resolve the DelayedWETH delay if not yet known.
        if self.weth_delay.is_none()
            && let Some(&game_address) = self.tracked.keys().next()
            && let Err(e) = self.resolve_weth_delay(verifier_client, game_address).await
        {
            warn!(error = %e, "failed to resolve DelayedWETH delay, will retry next poll");
        }

        let addresses: Vec<Address> = self.tracked.keys().copied().collect();
        let mut removed = Vec::new();

        for game_address in addresses {
            match self.advance_game(game_address, verifier_client, submitter).await {
                Ok(Some(reason)) => removed.push((game_address, reason)),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "failed to advance bond lifecycle"
                    );
                }
            }
        }

        for (addr, reason) in &removed {
            self.tracked.remove(addr);
            match reason {
                RemovalReason::Completed => {
                    ChallengerMetrics::bonds_completed_total().increment(1);
                }
                RemovalReason::NotClaimable => {
                    ChallengerMetrics::bonds_not_claimable_total().increment(1);
                }
            }
        }

        if !removed.is_empty() {
            ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        }
    }

    /// Advances a single game through the bond lifecycle state machine.
    ///
    /// Returns `Ok(Some(reason))` when the game should be removed from
    /// tracking, or `Ok(None)` when it remains in its current or updated
    /// phase.
    async fn advance_game(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<Option<RemovalReason>> {
        let game = match self.tracked.get(&game_address) {
            Some(g) => g,
            None => return Ok(None),
        };

        match &game.phase {
            BondPhase::NeedsResolve => {
                self.try_resolve(game_address, verifier_client, submitter).await
            }
            BondPhase::NeedsUnlock => {
                self.try_unlock(game_address, verifier_client, submitter).await
            }
            BondPhase::AwaitingDelay { unlocked_at } => {
                let unlocked_at = *unlocked_at;
                self.check_delay(game_address, unlocked_at)
            }
            BondPhase::NeedsWithdraw => {
                self.try_withdraw(game_address, verifier_client, submitter).await
            }
            BondPhase::Completed => Ok(Some(RemovalReason::Completed)),
        }
    }

    /// Attempts to resolve the game by calling `resolve()`.
    ///
    /// After resolution (either by us or by another actor), re-reads the
    /// onchain `bondRecipient` to verify it is still in our claim
    /// addresses. `resolve()` may update `bondRecipient` (e.g. to the
    /// challenger's address on `CHALLENGER_WINS`), so games matched via
    /// `zkProver` before resolution may no longer be claimable by us.
    async fn try_resolve(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<Option<RemovalReason>> {
        // Check if already resolved onchain (e.g., someone else called it).
        let status = verifier_client.status(game_address).await?;
        if status != GameScanner::STATUS_IN_PROGRESS {
            ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ALREADY_RESOLVED)
                .increment(1);

            // Re-read the onchain bondRecipient — resolve may have changed
            // it (e.g. to the challenger on CHALLENGER_WINS). If it is no
            // longer in our claim set, stop tracking this game.
            if !self.is_bond_claimable(verifier_client, game_address).await? {
                return Ok(Some(RemovalReason::NotClaimable));
            }

            info!(game = %game_address, status, "game already resolved, advancing to unlock phase");
            self.set_phase(game_address, BondPhase::NeedsUnlock);
            return Ok(None);
        }

        // Check if the game is ready to resolve.
        let game_over = verifier_client.game_over(game_address).await?;
        if !game_over {
            debug!(game = %game_address, "game dispute period not yet elapsed");
            return Ok(None);
        }

        // Submit resolve transaction.
        let calldata = encode_resolve_calldata();
        info!(game = %game_address, "submitting resolve transaction");
        match submitter.send_bond_tx(game_address, calldata).await {
            Ok(tx_hash) => {
                info!(game = %game_address, tx_hash = %tx_hash, "resolve transaction confirmed");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);

                // Re-read bondRecipient to verify it's in our claim set.
                if !self.is_bond_claimable(verifier_client, game_address).await? {
                    return Ok(Some(RemovalReason::NotClaimable));
                }

                self.set_phase(game_address, BondPhase::NeedsUnlock);
            }
            Err(e) => {
                warn!(game = %game_address, error = %e, "resolve transaction failed, will retry");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
            }
        }
        Ok(None)
    }

    /// Checks whether the onchain `bondRecipient` for the given game is in
    /// our claim addresses. Also updates the tracked game's
    /// `bond_recipient` field to reflect the current onchain value (which
    /// may differ from the pre-resolve value). Returns `false` if the
    /// recipient is not in the claim set, signalling the caller to remove
    /// the game from tracking.
    async fn is_bond_claimable(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
    ) -> eyre::Result<bool> {
        let bond_recipient = verifier_client.bond_recipient(game_address).await?;

        // Update the tracked entry so logging and debugging reflect the
        // current onchain recipient, not the stale pre-resolve value.
        if let Some(game) = self.tracked.get_mut(&game_address) {
            game.bond_recipient = bond_recipient;
        }

        if self.claim_addresses.contains(&bond_recipient) {
            return Ok(true);
        }
        info!(
            game = %game_address,
            recipient = %bond_recipient,
            "bond recipient not in claim addresses after resolve, removing from tracking"
        );
        Ok(false)
    }

    /// Attempts the first `claimCredit()` call to trigger the unlock.
    async fn try_unlock(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<Option<RemovalReason>> {
        // Check if already unlocked onchain.
        let unlocked = verifier_client.bond_unlocked(game_address).await?;
        if unlocked {
            // Use `resolved_at` as a conservative lower bound for the unlock
            // time. The unlock must have occurred after resolve, so this may
            // cause one early withdrawal attempt that reverts, but is strictly
            // better than resetting to "now" (which would re-impose the full
            // delay after every restart).
            let resolved_at = verifier_client.resolved_at(game_address).await?;
            let unlocked_at =
                Self::unix_to_monotonic(&self.clock, resolved_at, Self::wall_clock_unix_secs());
            info!(
                game = %game_address,
                resolved_at,
                "bond already unlocked, advancing to delay phase"
            );
            self.set_phase(game_address, BondPhase::AwaitingDelay { unlocked_at });
            return Ok(None);
        }

        self.submit_claim_credit(
            game_address,
            submitter,
            "unlock",
            BondPhase::AwaitingDelay { unlocked_at: self.clock.now() },
        )
        .await
    }

    /// Checks if the `DelayedWETH` delay has elapsed since the unlock.
    fn check_delay(
        &mut self,
        game_address: Address,
        unlocked_at: Duration,
    ) -> eyre::Result<Option<RemovalReason>> {
        // Fall back to 7 days if the onchain delay has not been read yet.
        // If the real delay is shorter, the withdraw attempt will simply
        // succeed earlier than expected. If longer, the attempt will revert
        // and be retried on the next poll tick.
        let delay = self.weth_delay.unwrap_or_else(|| {
            warn!(game = %game_address, "WETH delay not yet known, using default 7 days");
            Self::DEFAULT_WETH_DELAY
        });

        let elapsed = self.clock.now().saturating_sub(unlocked_at);

        if elapsed >= delay {
            info!(
                game = %game_address,
                elapsed_secs = elapsed.as_secs(),
                "DelayedWETH delay elapsed, advancing to withdraw phase"
            );
            self.set_phase(game_address, BondPhase::NeedsWithdraw);
        } else {
            let remaining = delay.saturating_sub(elapsed);
            debug!(
                game = %game_address,
                remaining_secs = remaining.as_secs(),
                "waiting for DelayedWETH delay"
            );
        }
        Ok(None)
    }

    /// Attempts the second `claimCredit()` call to complete the withdrawal.
    async fn try_withdraw(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<Option<RemovalReason>> {
        // Check if already claimed onchain.
        let claimed = verifier_client.bond_claimed(game_address).await?;
        if claimed {
            info!(game = %game_address, "bond already claimed");
            self.set_phase(game_address, BondPhase::Completed);
            return Ok(Some(RemovalReason::Completed));
        }

        self.submit_claim_credit(game_address, submitter, "withdraw", BondPhase::Completed).await
    }

    /// Submits a `claimCredit()` transaction and transitions to the given
    /// phase on success. Returns `Ok(Some(Completed))` when the success
    /// phase is [`BondPhase::Completed`].
    async fn submit_claim_credit(
        &mut self,
        game_address: Address,
        submitter: &dyn BondTransactionSubmitter,
        step: &str,
        success_phase: BondPhase,
    ) -> eyre::Result<Option<RemovalReason>> {
        let calldata = encode_claim_credit_calldata();
        ChallengerMetrics::claim_credit_tx_submitted_total().increment(1);
        info!(game = %game_address, step, "submitting claimCredit transaction");
        match submitter.send_bond_tx(game_address, calldata).await {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    tx_hash = %tx_hash,
                    step,
                    "claimCredit transaction confirmed"
                );
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
                let completed = matches!(success_phase, BondPhase::Completed);
                self.set_phase(game_address, success_phase);
                Ok(completed.then_some(RemovalReason::Completed))
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    error = %e,
                    step,
                    "claimCredit transaction failed, will retry"
                );
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                Ok(None)
            }
        }
    }

    /// Converts a Unix timestamp (seconds) to a monotonic [`Duration`]
    /// relative to the given clock.
    ///
    /// Computes how long ago `unix_secs` occurred relative to `unix_now`
    /// and subtracts that age from the current monotonic time. Used when
    /// recovering on-chain timestamps (e.g. `resolved_at`) into the
    /// local monotonic time domain.
    ///
    /// `unix_now` is accepted as a parameter (rather than calling
    /// `SystemTime::now()` internally) so that the function is fully
    /// deterministic and testable.
    ///
    /// If `unix_secs` is ahead of `unix_now` (e.g. L1 clock skew),
    /// the age is treated as zero and `unlocked_at` equals the current
    /// monotonic time — re-imposing the full delay. This is the safe
    /// conservative fallback.
    fn unix_to_monotonic(clock: &C, unix_secs: u64, unix_now: u64) -> Duration {
        let age = Duration::from_secs(unix_now.saturating_sub(unix_secs));
        clock.now().saturating_sub(age)
    }

    /// Returns the current Unix timestamp in seconds.
    fn wall_clock_unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs()
    }

    /// Determines the bond phase from onchain state.
    ///
    /// Returns `None` if the bond has already been fully claimed. Otherwise
    /// returns the appropriate [`BondPhase`] based on the game's onchain
    /// progression (resolved, unlocked, etc.). The caller is responsible
    /// for verifying that the onchain `bondRecipient` is in the claim set
    /// before acting on the returned phase.
    async fn determine_phase(
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
        clock: &C,
    ) -> eyre::Result<Option<BondPhase>> {
        let bond_claimed = verifier_client.bond_claimed(game_address).await?;
        if bond_claimed {
            return Ok(None);
        }

        let resolved_at = verifier_client.resolved_at(game_address).await?;

        let bond_unlocked = verifier_client.bond_unlocked(game_address).await?;
        if bond_unlocked {
            // Use `resolved_at` as a conservative lower bound for the unlock
            // time. The unlock must have occurred after resolve, so this may
            // cause one early withdrawal attempt that reverts, but is strictly
            // better than resetting to "now" (which would re-impose the full
            // delay after every restart).
            let unlocked_at =
                Self::unix_to_monotonic(clock, resolved_at, Self::wall_clock_unix_secs());
            return Ok(Some(BondPhase::AwaitingDelay { unlocked_at }));
        }

        if resolved_at > 0 {
            return Ok(Some(BondPhase::NeedsUnlock));
        }

        Ok(Some(BondPhase::NeedsResolve))
    }

    /// Reads the `DelayedWETH` address from a game proxy and fetches the delay.
    async fn resolve_weth_delay(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
    ) -> eyre::Result<()> {
        let weth_address = verifier_client.delayed_weth(game_address).await?;
        let weth_client = DelayedWETHContractClient::new(weth_address, self.l1_rpc_url.clone())?;
        let delay = weth_client.delay().await?;
        self.set_weth_delay(delay);
        Ok(())
    }
}

/// Trait for submitting bond lifecycle transactions (resolve, claimCredit).
///
/// This abstracts the transaction submission layer so the [`BondManager`]
/// can be tested with mock submitters.
#[async_trait::async_trait]
pub trait BondTransactionSubmitter: Send + Sync {
    /// Sends a transaction with the given calldata to the game address.
    ///
    /// Returns the transaction hash on success.
    async fn send_bond_tx(
        &self,
        game_address: Address,
        calldata: alloy_primitives::Bytes,
    ) -> Result<alloy_primitives::B256, crate::ChallengeSubmitError>;
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin};

    use futures::stream::BoxStream;

    use super::*;
    use crate::test_utils::{MockDisputeGameFactory, empty_factory};

    /// A deterministic clock that always returns a fixed [`Duration`].
    ///
    /// Used in unit tests that need precise control over the monotonic
    /// time returned by [`Clock::now`].
    struct FixedClock(Duration);

    impl Clock for FixedClock {
        fn now(&self) -> Duration {
            self.0
        }

        fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::pending())
        }

        fn interval(&self, _period: Duration) -> BoxStream<'static, ()> {
            Box::pin(futures::stream::pending())
        }
    }

    fn test_l1_rpc_url() -> url::Url {
        "http://localhost:8545".parse().unwrap()
    }

    fn make_manager(addresses: Vec<Address>) -> BondManager<FixedClock> {
        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(addresses, test_l1_rpc_url(), empty_factory(), 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));
        mgr
    }

    #[test]
    fn track_game_filters_by_claim_address() {
        let addr = Address::repeat_byte(0x01);
        let other = Address::repeat_byte(0x02);
        let game = Address::repeat_byte(0xAA);

        let mut mgr = make_manager(vec![addr]);
        assert!(mgr.track_game(game, addr));
        assert!(!mgr.track_game(game, addr)); // duplicate
        assert!(!mgr.track_game(Address::repeat_byte(0xBB), other)); // not in set
    }

    #[test]
    fn is_tracking_returns_correct_state() {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let mut mgr = make_manager(vec![addr]);
        assert!(!mgr.is_tracking(&game));
        mgr.track_game(game, addr);
        assert!(mgr.is_tracking(&game));
    }

    #[test]
    fn check_delay_transitions_when_elapsed() {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let clock = FixedClock(Duration::from_secs(1000));
        let mut mgr = BondManager::new(vec![addr], test_l1_rpc_url(), empty_factory(), 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        // 100 seconds ago > 60 second delay
        let unlocked_at = Duration::from_secs(900);
        mgr.tracked.insert(
            game,
            TrackedGame { phase: BondPhase::AwaitingDelay { unlocked_at }, bond_recipient: addr },
        );

        let result = mgr.check_delay(game, unlocked_at);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::NeedsWithdraw));
    }

    #[test]
    fn check_delay_stays_when_not_elapsed() {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let clock = FixedClock(Duration::from_secs(1000));
        let mut mgr = BondManager::new(vec![addr], test_l1_rpc_url(), empty_factory(), 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(3600));

        // only 1 second ago < 3600 second delay
        let unlocked_at = Duration::from_secs(999);
        mgr.tracked.insert(
            game,
            TrackedGame { phase: BondPhase::AwaitingDelay { unlocked_at }, bond_recipient: addr },
        );

        let result = mgr.check_delay(game, unlocked_at);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::AwaitingDelay { .. }));
    }

    #[test]
    fn unix_to_monotonic_past_timestamp() {
        // Clock at 500s monotonic, unix_now=2000, event at unix 1900
        // → age = 100s → monotonic = 500 - 100 = 400s.
        let clock = FixedClock(Duration::from_secs(500));
        let result = BondManager::unix_to_monotonic(&clock, 1900, 2000);
        assert_eq!(result, Duration::from_secs(400));
    }

    #[test]
    fn unix_to_monotonic_future_timestamp_clamps() {
        // If the on-chain timestamp is ahead of local wall clock
        // (clock skew), age saturates to 0 → monotonic = clock.now().
        let clock = FixedClock(Duration::from_secs(500));
        let result = BondManager::unix_to_monotonic(&clock, 2100, 2000);
        assert_eq!(result, Duration::from_secs(500));
    }

    #[test]
    fn unix_to_monotonic_same_timestamp() {
        // Event happened "right now" → age = 0 → monotonic = clock.now().
        let clock = FixedClock(Duration::from_secs(500));
        let result = BondManager::unix_to_monotonic(&clock, 2000, 2000);
        assert_eq!(result, Duration::from_secs(500));
    }

    #[test]
    fn unix_to_monotonic_age_exceeds_monotonic() {
        // If the event is older than the monotonic uptime, saturate to zero
        // rather than underflowing.
        let clock = FixedClock(Duration::from_secs(50));
        let result = BondManager::unix_to_monotonic(&clock, 1000, 2000);
        assert_eq!(result, Duration::ZERO);
    }

    #[test]
    fn empty_claim_addresses_means_disabled() {
        let clock = FixedClock(Duration::from_secs(0));
        let mgr = BondManager::new(vec![], test_l1_rpc_url(), empty_factory(), 1000, clock);
        assert!(!mgr.is_enabled());
    }

    #[test]
    fn non_empty_claim_addresses_means_enabled() {
        let clock = FixedClock(Duration::from_secs(0));
        let mgr = BondManager::new(
            vec![Address::repeat_byte(0x01)],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            clock,
        );
        assert!(mgr.is_enabled());
    }

    // ---- discover_claimable_games tests ----

    use crate::test_utils::{MockAggregateVerifier, addr, factory_game, mock_state};

    /// Builds a factory and verifier pair where each game has the given
    /// `bond_recipient` and `zk_prover`. All games are `IN_PROGRESS` (status 0)
    /// unless overridden.
    fn discovery_mocks(
        game_count: u64,
        bond_recipient: Address,
        zk_prover: Address,
    ) -> (Arc<dyn DisputeGameFactoryClient>, Arc<MockAggregateVerifier>) {
        let games: Vec<_> = (0..game_count).map(|i| factory_game(i, 0)).collect();
        let mut verifier_games = HashMap::new();
        for i in 0..game_count {
            let mut state = mock_state(0, zk_prover, 100 + i);
            state.bond_recipient = bond_recipient;
            verifier_games.insert(addr(i), state);
        }
        let factory: Arc<dyn DisputeGameFactoryClient> = Arc::new(MockDisputeGameFactory { games });
        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });
        (factory, verifier)
    }

    #[tokio::test]
    async fn discover_incremental_picks_up_new_games_by_recipient() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(3, claim_addr, Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        // bond_scan_head defaults to 0, so the first call should scan all 3.
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 3);
        assert_eq!(mgr.bond_scan_head, 3);
    }

    #[tokio::test]
    async fn discover_incremental_picks_up_new_games_by_zk_prover() {
        let claim_addr = Address::repeat_byte(0xCC);
        let other_recipient = Address::repeat_byte(0xDD);
        // bond_recipient is someone else, but zkProver matches our address.
        // Status 0 = IN_PROGRESS, so the game should match via zkProver.
        let (factory, verifier) = discovery_mocks(2, other_recipient, claim_addr);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 2);
    }

    #[tokio::test]
    async fn discover_skips_already_tracked_games() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(2, claim_addr, Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        // Pre-track game 0.
        mgr.track_game(addr(0), claim_addr);
        assert_eq!(mgr.tracked_count(), 1);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        // Game 0 was already tracked, so only game 1 should be new.
        assert_eq!(mgr.tracked_count(), 2);
    }

    #[tokio::test]
    async fn discover_skips_already_claimed_games() {
        let claim_addr = Address::repeat_byte(0xCC);

        let games = vec![factory_game(0, 0)];
        let mut verifier_games = HashMap::new();
        let mut state = mock_state(1, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.bond_claimed = true; // already claimed
        state.resolved_at = 500;
        verifier_games.insert(addr(0), state);

        let factory: Arc<dyn DisputeGameFactoryClient> = Arc::new(MockDisputeGameFactory { games });
        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0, "claimed game should not be tracked");
    }

    #[tokio::test]
    async fn discover_advances_watermark() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(5, claim_addr, Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        // Start from index 3 so only indices 3 and 4 are scanned.
        mgr.bond_scan_head = 3;
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.bond_scan_head, 5);
        assert_eq!(mgr.tracked_count(), 2, "only games 3 and 4 should be discovered");
    }

    #[tokio::test]
    async fn discover_noop_when_watermark_equals_game_count() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(5, claim_addr, Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        // Watermark already at game_count — nothing new to scan.
        mgr.bond_scan_head = 5;
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        assert_eq!(mgr.bond_scan_head, 5);
    }

    #[tokio::test]
    async fn discover_full_rescan_resets_watermark() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(10, claim_addr, Address::ZERO);

        // Use a clock at 1000s so we can backdate last_full_scan.
        let clock = FixedClock(Duration::from_secs(1000));
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            5, // lookback = 5
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        // Simulate that the previous scan already covered everything.
        mgr.bond_scan_head = 10;

        // Force the full rescan by backdating `last_full_scan` past the
        // discovery interval.
        mgr.last_full_scan = Duration::from_secs(1000)
            .saturating_sub(BondManager::<FixedClock>::BOND_DISCOVERY_INTERVAL);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        // Full rescan should have reset watermark to 10 - 5 = 5
        // and then scanned indices 5..10, discovering 5 new games.
        assert_eq!(mgr.bond_scan_head, 10);
        assert_eq!(mgr.tracked_count(), 5);
    }

    #[tokio::test]
    async fn discover_disabled_when_no_claim_addresses() {
        let (_, verifier) = discovery_mocks(5, Address::repeat_byte(0xCC), Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![], test_l1_rpc_url(), empty_factory(), 1000, clock);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
    }

    #[tokio::test]
    async fn discover_skips_unmatched_recipients() {
        let claim_addr = Address::repeat_byte(0xCC);
        let other = Address::repeat_byte(0xDD);
        // Neither bondRecipient nor zkProver match our claim address.
        let (factory, verifier) = discovery_mocks(3, other, Address::ZERO);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr = BondManager::new(vec![claim_addr], test_l1_rpc_url(), factory, 1000, clock);
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        // Watermark should still advance past the scanned range.
        assert_eq!(mgr.bond_scan_head, 3);
    }

    #[tokio::test]
    async fn discover_handles_empty_factory() {
        let claim_addr = Address::repeat_byte(0xCC);

        let clock = FixedClock(Duration::from_secs(0));
        let mut mgr =
            BondManager::new(vec![claim_addr], test_l1_rpc_url(), empty_factory(), 1000, clock);

        let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        assert_eq!(mgr.bond_scan_head, 0);
    }
}
