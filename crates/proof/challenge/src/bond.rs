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
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, encode_claim_credit_calldata, encode_resolve_calldata,
    encode_set_anchor_state_calldata,
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
    /// Whether the anchor state update has been completed (or skipped)
    /// for this game. Set to `true` after a successful
    /// `setAnchorState()` call or when the game is not eligible
    /// (e.g. `CHALLENGER_WINS`).
    pub anchor_update_complete: bool,
    /// Cached on-chain game status (`0` = in progress, `1` = challenger
    /// wins, `2` = defender wins). Populated on the first successful
    /// `status()` RPC read; subsequent ticks reuse the cached value
    /// because the status is immutable after resolution.
    pub cached_status: Option<u8>,
    /// Cached `AnchorStateRegistry` address for this game. Read from the
    /// game contract on first use and reused thereafter (immutable per game).
    pub cached_asr_address: Option<Address>,
    /// Cached L2 block number for this game. Read from immutable game info
    /// once the game is finalized and reused for retries.
    pub cached_l2_block_number: Option<u64>,
    /// Monotonic timestamp for games whose bond lifecycle is complete but
    /// which remain tracked until the anchor update completes.
    pub anchor_update_retained_since: Option<Duration>,
    /// Removal reason to use once a retained game is finally evicted.
    pub anchor_update_retention_reason: Option<RemovalReason>,
}

impl TrackedGame {
    /// Creates a freshly-tracked game in the given phase. All caches and
    /// retention timestamps start unset.
    pub const fn new(phase: BondPhase, bond_recipient: Address) -> Self {
        Self {
            phase,
            bond_recipient,
            anchor_update_complete: false,
            cached_status: None,
            cached_asr_address: None,
            cached_l2_block_number: None,
            anchor_update_retained_since: None,
            anchor_update_retention_reason: None,
        }
    }
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
    /// How often a full rescan of the lookback window is performed to catch
    /// state transitions (games challenged or resolved by other actors).
    discovery_interval: Duration,
    /// Maximum time to retain a completed bond game while waiting for its
    /// anchor update to complete.
    anchor_update_retention: Duration,
}

impl<C: Clock> BondManager<C> {
    /// Conservative fallback when the onchain `DelayedWETH` delay has not
    /// been read yet. If the real delay is shorter the withdraw will simply
    /// succeed earlier; if longer, the attempt reverts and is retried.
    const DEFAULT_WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Default maximum time to keep a completed bond game tracked while
    /// waiting for its best-effort anchor update to finish.
    const DEFAULT_ANCHOR_UPDATE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

    /// `DEFENDER_WINS` game status constant.
    const STATUS_DEFENDER_WINS: u8 = 2;

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
            anchor_update_retention: Self::DEFAULT_ANCHOR_UPDATE_RETENTION,
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

    /// Sets the maximum time a completed bond game remains tracked while
    /// waiting for its anchor update to complete.
    pub fn set_anchor_update_retention(&mut self, retention: Duration) {
        info!(retention_secs = retention.as_secs(), "anchor update retention configured");
        self.anchor_update_retention = retention;
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
            .insert(game_address, TrackedGame::new(BondPhase::NeedsResolve, bond_recipient));
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
        factory_client: &dyn DisputeGameFactoryClient,
        verifier_client: &dyn AggregateVerifierClient,
        claim_addresses: &HashSet<Address>,
        clock: &C,
    ) -> Vec<Option<(Address, Address, Option<BondPhase>)>> {
        stream::iter(range)
            .map(|i| async move {
                Self::evaluate_game_for_bonds(
                    i,
                    factory_client,
                    verifier_client,
                    claim_addresses,
                    clock,
                )
                .await
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

        let game_count = self.factory_client.game_count().await?;
        if game_count == 0 {
            info!("no games in factory, skipping bond startup scan");
            return Ok(());
        }

        let start_index = game_count.saturating_sub(self.lookback);
        info!(start = start_index, end = game_count, "scanning recent games for bond recovery");

        let results = Self::evaluate_bond_range(
            start_index..game_count,
            &*self.factory_client,
            verifier_client,
            &self.claim_addresses,
            &self.clock,
        )
        .await;

        // Resolve the WETH delay from the first available game so the
        // delay is bootstrapped as early as possible.
        if let Some((game_address, _, _)) = results.iter().flatten().next() {
            self.ensure_weth_delay(verifier_client, *game_address).await;
        }

        for (game_address, bond_recipient, phase) in results.into_iter().flatten() {
            let Some(phase) = phase else {
                continue;
            };

            info!(
                game = %game_address,
                recipient = %bond_recipient,
                phase = ?phase,
                "recovered game for bond tracking"
            );
            self.tracked.insert(game_address, TrackedGame::new(phase, bond_recipient));
        }

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
    /// **Periodic full rescan** (every `discovery_interval`):
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

        let game_count = self.factory_client.game_count().await?;
        if game_count == 0 {
            debug!("no games found, skipping bond discovery scan");
            return Ok(());
        }

        // Periodic full rescan: reset watermark to re-evaluate the
        // lookback window and catch state transitions on older games.
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

        let results = Self::evaluate_bond_range(
            scan_start..scan_end,
            &*self.factory_client,
            verifier_client,
            &self.claim_addresses,
            &self.clock,
        )
        .await;

        let mut discovered = 0u64;

        for (game_address, bond_recipient, phase) in results.into_iter().flatten() {
            if self.tracked.contains_key(&game_address) {
                continue;
            }

            let Some(phase) = phase else {
                continue;
            };

            info!(
                game = %game_address,
                recipient = %bond_recipient,
                phase = ?phase,
                scan_type,
                "discovered claimable game"
            );
            self.tracked.insert(game_address, TrackedGame::new(phase, bond_recipient));
            discovered += 1;
        }

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
        {
            self.ensure_weth_delay(verifier_client, game_address).await;
        }

        let addresses: Vec<Address> = self.tracked.keys().copied().collect();
        let mut removed = Vec::new();

        for game_address in addresses {
            if let Some(reason) =
                self.tracked.get(&game_address).and_then(|g| g.anchor_update_retention_reason)
            {
                self.try_anchor_update(game_address, verifier_client, submitter).await;
                if self.handle_lifecycle_completion(game_address, reason) {
                    removed.push((game_address, reason));
                }
                continue;
            }

            match self.advance_game(game_address, verifier_client, submitter).await {
                Ok(Some(reason)) => {
                    self.try_anchor_update(game_address, verifier_client, submitter).await;
                    if self.handle_lifecycle_completion(game_address, reason) {
                        removed.push((game_address, reason));
                    }
                }
                Ok(None) => {
                    self.try_anchor_update(game_address, verifier_client, submitter).await;
                }
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
        let retained =
            self.tracked.values().filter(|g| g.anchor_update_retained_since.is_some()).count();
        ChallengerMetrics::anchor_update_retained_games().set(retained as f64);
    }

    /// Decides whether a game whose bond lifecycle has finished should be
    /// removed now or retained until [`try_anchor_update`] succeeds.
    ///
    /// Returns `true` if the caller should remove the game from tracking.
    fn handle_lifecycle_completion(
        &mut self,
        game_address: Address,
        reason: RemovalReason,
    ) -> bool {
        let now = self.clock.now();
        let Some(game) = self.tracked.get_mut(&game_address) else {
            return true;
        };
        if game.anchor_update_complete {
            return true;
        }
        if let Some(retained_since) = game.anchor_update_retained_since {
            game.anchor_update_retention_reason = Some(reason);
            let retained_duration = now.saturating_sub(retained_since);
            if retained_duration >= self.anchor_update_retention {
                warn!(
                    game = %game_address,
                    reason = ?reason,
                    retained_secs = retained_duration.as_secs(),
                    retention_secs = self.anchor_update_retention.as_secs(),
                    "evicting game after anchor update retention timeout"
                );
                return true;
            }
            debug!(
                game = %game_address,
                reason = ?reason,
                retained_secs = retained_duration.as_secs(),
                "keeping game tracked until anchor update completes"
            );
            return false;
        }
        game.anchor_update_retained_since = Some(now);
        game.anchor_update_retention_reason = Some(reason);
        warn!(
            game = %game_address,
            reason = ?reason,
            "retaining completed bond game until anchor update completes"
        );
        ChallengerMetrics::anchor_update_retained_games_total().increment(1);
        false
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
                self.check_delay(game_address, *unlocked_at)
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
        let status = verifier_client.status(game_address).await?;

        if status == GameScanner::STATUS_IN_PROGRESS {
            let game_over = verifier_client.game_over(game_address).await?;
            if !game_over {
                debug!(game = %game_address, "game dispute period not yet elapsed");
                return Ok(None);
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
                    // Re-read and cache the now-immutable status so that
                    // try_anchor_update (called later this tick) can use
                    // it without a redundant RPC call.
                    match verifier_client.status(game_address).await {
                        Ok(resolved_status) => {
                            if let Some(g) = self.tracked.get_mut(&game_address) {
                                g.cached_status = Some(resolved_status);
                            }
                        }
                        Err(e) => {
                            debug!(
                                game = %game_address,
                                error = %e,
                                "failed to cache status after resolve, will re-read later"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "resolve transaction failed, will retry"
                    );
                    ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                        .increment(1);
                    return Ok(None);
                }
            }
        } else {
            ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ALREADY_RESOLVED)
                .increment(1);
            info!(game = %game_address, status, "game already resolved");
            // Status is immutable after resolution — cache it so that
            // try_anchor_update (called later this tick) can skip the
            // redundant RPC round-trip.
            if let Some(g) = self.tracked.get_mut(&game_address) {
                g.cached_status = Some(status);
            }
        }

        // Re-read the onchain bondRecipient — resolve may have changed it
        // (e.g. to the challenger on CHALLENGER_WINS). If it is no longer
        // in our claim set, stop tracking this game.
        if !self.is_bond_claimable(verifier_client, game_address).await? {
            return Ok(Some(RemovalReason::NotClaimable));
        }

        self.set_phase(game_address, BondPhase::NeedsUnlock);
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
        let (unlocked, resolved_at) = futures::try_join!(
            verifier_client.bond_unlocked(game_address),
            verifier_client.resolved_at(game_address),
        )?;
        if unlocked {
            let unlocked_at = Self::estimate_unlock_time(&self.clock, resolved_at);
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
            debug!(game = %game_address, "WETH delay not yet known, using default 7 days");
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
        let claimed = verifier_client.bond_claimed(game_address).await?;
        if claimed {
            info!(game = %game_address, "bond already claimed");
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
        match submitter.send_bond_tx(game_address, game_address, calldata).await {
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
    /// `unix_now` is accepted as a parameter (callers obtain it via
    /// [`Clock::wall_clock_unix_secs`]) so that the function is fully
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

    /// Estimates when the bond was unlocked using `resolved_at` as a
    /// conservative lower bound. The unlock must have occurred after
    /// resolve, so this may cause one early withdrawal attempt that
    /// reverts, but is strictly better than resetting to "now" (which
    /// would re-impose the full delay after every restart).
    fn estimate_unlock_time(clock: &C, resolved_at: u64) -> Duration {
        Self::unix_to_monotonic(clock, resolved_at, clock.wall_clock_unix_secs())
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
        let (bond_claimed, resolved_at, bond_unlocked) = futures::try_join!(
            verifier_client.bond_claimed(game_address),
            verifier_client.resolved_at(game_address),
            verifier_client.bond_unlocked(game_address),
        )?;
        if bond_claimed {
            return Ok(None);
        }
        if bond_unlocked {
            let unlocked_at = Self::estimate_unlock_time(clock, resolved_at);
            return Ok(Some(BondPhase::AwaitingDelay { unlocked_at }));
        }

        if resolved_at > 0 {
            return Ok(Some(BondPhase::NeedsUnlock));
        }

        Ok(Some(BondPhase::NeedsResolve))
    }

    /// Resolves the `DelayedWETH` delay if not yet known, logging on failure.
    async fn ensure_weth_delay(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
    ) {
        if self.weth_delay.is_none()
            && let Err(e) = self.resolve_weth_delay(verifier_client, game_address).await
        {
            warn!(error = %e, "failed to read DelayedWETH delay, will retry later");
        }
    }

    /// Marks the anchor update as complete for a tracked game. No-op if the
    /// game is no longer tracked.
    fn mark_anchor_update_complete(&mut self, game_address: Address) {
        if let Some(game) = self.tracked.get_mut(&game_address) {
            game.anchor_update_complete = true;
        }
    }

    /// Marks an anchor update as permanently skipped: the game cannot become
    /// a valid anchor and we increment the SKIPPED outcome metric.
    fn skip_anchor_update_permanently(&mut self, game_address: Address) {
        self.mark_anchor_update_complete(game_address);
        ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_SKIPPED)
            .increment(1);
    }

    /// Best-effort attempt to update the `AnchorStateRegistry` for a
    /// resolved game.
    ///
    /// Skips updates for games that are permanently ineligible (blacklisted,
    /// retired, or already stale relative to the current anchor) so the tx
    /// submitter is not spammed each poll tick. Transient ineligibility
    /// (airgap delay not elapsed, not yet finalized, or currently
    /// unrespected) is left to retry on the next tick — the on-chain
    /// `setAnchorState()` call is permissionless and self-validating.
    async fn try_anchor_update(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) {
        let game = match self.tracked.get(&game_address) {
            Some(g) => g,
            None => return,
        };

        if game.anchor_update_complete
            || (game.cached_status.is_none() && matches!(game.phase, BondPhase::NeedsResolve))
        {
            return;
        }

        // Only DEFENDER_WINS games can update the anchor state.
        // The status is immutable after resolution, so we cache it
        // after the first successful RPC read to avoid redundant calls.
        let status = if let Some(cached) = game.cached_status {
            cached
        } else {
            match verifier_client.status(game_address).await {
                Ok(s) => {
                    if let Some(g) = self.tracked.get_mut(&game_address) {
                        g.cached_status = Some(s);
                    }
                    s
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read status for anchor update"
                    );
                    return;
                }
            }
        };

        if status != Self::STATUS_DEFENDER_WINS {
            self.skip_anchor_update_permanently(game_address);
            return;
        }

        // Resolve the ASR address from the game contract (cached after first read).
        let asr_address = if let Some(cached) =
            self.tracked.get(&game_address).and_then(|g| g.cached_asr_address)
        {
            cached
        } else {
            match verifier_client.anchor_state_registry(game_address).await {
                Ok(addr) => {
                    if let Some(g) = self.tracked.get_mut(&game_address) {
                        g.cached_asr_address = Some(addr);
                    }
                    addr
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "failed to read anchorStateRegistry for anchor update"
                    );
                    return;
                }
            }
        };

        // Keep the cheap finality read before the heavier preflight batch. With
        // a nonzero ASR airgap, blacklisted/retired games may retry this single
        // read until finality, but moving preflight earlier would add several
        // reads for every ordinary not-yet-finalized game.
        match verifier_client.is_game_finalized(asr_address, game_address).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    "anchor state update not ready because game is not finalized"
                );
                return;
            }
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "failed to read isGameFinalized, will retry"
                );
                return;
            }
        }

        let preflight = match verifier_client.anchor_preflight(asr_address, game_address).await {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "failed to read anchor preflight state, will retry"
                );
                return;
            }
        };

        if preflight.permanently_ineligible() {
            info!(
                game = %game_address,
                asr = %asr_address,
                blacklisted = preflight.blacklisted,
                retired = preflight.retired,
                respected = preflight.respected,
                "skipping permanently ineligible anchor update"
            );
            self.skip_anchor_update_permanently(game_address);
            return;
        }

        if !preflight.respected {
            debug!(
                game = %game_address,
                asr = %asr_address,
                "anchor state update not ready because game is not currently respected"
            );
            return;
        }

        let game_l2_block_number = if let Some(cached) =
            self.tracked.get(&game_address).and_then(|g| g.cached_l2_block_number)
        {
            cached
        } else {
            match verifier_client.game_info(game_address).await {
                Ok(info) => {
                    if let Some(g) = self.tracked.get_mut(&game_address) {
                        g.cached_l2_block_number = Some(info.l2_block_number);
                    }
                    info.l2_block_number
                }
                Err(e) => {
                    debug!(
                        game = %game_address,
                        asr = %asr_address,
                        error = %e,
                        "failed to read game info for anchor preflight, will retry"
                    );
                    return;
                }
            }
        };

        if game_l2_block_number <= preflight.anchor_root.l2_block_number {
            info!(
                game = %game_address,
                asr = %asr_address,
                game_l2_block = game_l2_block_number,
                anchor_l2_block = preflight.anchor_root.l2_block_number,
                "skipping stale anchor state update"
            );
            self.skip_anchor_update_permanently(game_address);
            return;
        }

        let calldata = encode_set_anchor_state_calldata(game_address);
        match submitter.send_bond_tx(game_address, asr_address, calldata).await {
            Ok(tx_hash) => {
                info!(
                    game = %game_address,
                    asr = %asr_address,
                    tx_hash = %tx_hash,
                    "anchor state registry updated"
                );
                self.mark_anchor_update_complete(game_address);
                ChallengerMetrics::anchor_update_tx_outcome_total(
                    ChallengerMetrics::STATUS_SUCCESS,
                )
                .increment(1);
                ChallengerMetrics::anchor_l2_block_number().set(game_l2_block_number as f64);
            }
            Err(e) => {
                debug!(
                    game = %game_address,
                    asr = %asr_address,
                    error = %e,
                    "anchor state update failed, will retry"
                );
                ChallengerMetrics::anchor_update_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
            }
        }
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

/// Trait for submitting bond lifecycle transactions (resolve, claimCredit,
/// setAnchorState).
///
/// This abstracts the transaction submission layer so the [`BondManager`]
/// can be tested with mock submitters.
#[async_trait::async_trait]
pub trait BondTransactionSubmitter: Send + Sync {
    /// Sends a transaction with the given calldata to `to`. `game_address`
    /// is the dispute game this transaction is associated with, used for
    /// log/metric correlation.
    async fn send_bond_tx(
        &self,
        game_address: Address,
        to: Address,
        calldata: alloy_primitives::Bytes,
    ) -> Result<alloy_primitives::B256, crate::ChallengeSubmitError>;
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin};

    use alloy_primitives::B256;
    use futures::stream::BoxStream;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockBondTransactionSubmitter, MockDisputeGameFactory, MockGameState,
        TEST_DISCOVERY_INTERVAL, addr, empty_factory, factory_game, mock_state,
    };

    /// A deterministic clock that always returns fixed values.
    ///
    /// Used in unit tests that need precise control over the monotonic
    /// time returned by [`Clock::now`] and the wall-clock Unix seconds
    /// returned by [`Clock::wall_clock_unix_secs`].
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

    /// Creates a [`FixedClock`] with the given monotonic seconds and a
    /// default wall-clock value of `2_000_000_000`.
    fn fixed_clock(secs: u64) -> FixedClock {
        FixedClock { monotonic: Duration::from_secs(secs), wall_unix: 2_000_000_000 }
    }

    fn test_l1_rpc_url() -> url::Url {
        "http://localhost:8545".parse().unwrap()
    }

    fn make_manager(addresses: Vec<Address>) -> BondManager<FixedClock> {
        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            addresses,
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
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

        let clock = fixed_clock(1000);
        let mut mgr = BondManager::new(
            vec![addr],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        // 100 seconds ago > 60 second delay
        let unlocked_at = Duration::from_secs(900);
        mgr.tracked.insert(game, TrackedGame::new(BondPhase::AwaitingDelay { unlocked_at }, addr));

        let result = mgr.check_delay(game, unlocked_at);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::NeedsWithdraw));
    }

    #[test]
    fn check_delay_stays_when_not_elapsed() {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let clock = fixed_clock(1000);
        let mut mgr = BondManager::new(
            vec![addr],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(3600));

        // only 1 second ago < 3600 second delay
        let unlocked_at = Duration::from_secs(999);
        mgr.tracked.insert(game, TrackedGame::new(BondPhase::AwaitingDelay { unlocked_at }, addr));

        let result = mgr.check_delay(game, unlocked_at);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::AwaitingDelay { .. }));
    }

    #[test]
    fn unix_to_monotonic_past_timestamp() {
        // Clock at 500s monotonic, unix_now=2000, event at unix 1900
        // → age = 100s → monotonic = 500 - 100 = 400s.
        let clock = fixed_clock(500);
        let result = BondManager::unix_to_monotonic(&clock, 1900, 2000);
        assert_eq!(result, Duration::from_secs(400));
    }

    #[test]
    fn unix_to_monotonic_future_timestamp_clamps() {
        // If the on-chain timestamp is ahead of local wall clock
        // (clock skew), age saturates to 0 → monotonic = clock.now().
        let clock = fixed_clock(500);
        let result = BondManager::unix_to_monotonic(&clock, 2100, 2000);
        assert_eq!(result, Duration::from_secs(500));
    }

    #[test]
    fn unix_to_monotonic_same_timestamp() {
        // Event happened "right now" → age = 0 → monotonic = clock.now().
        let clock = fixed_clock(500);
        let result = BondManager::unix_to_monotonic(&clock, 2000, 2000);
        assert_eq!(result, Duration::from_secs(500));
    }

    #[test]
    fn unix_to_monotonic_age_exceeds_monotonic() {
        // If the event is older than the monotonic uptime, saturate to zero
        // rather than underflowing.
        let clock = fixed_clock(50);
        let result = BondManager::unix_to_monotonic(&clock, 1000, 2000);
        assert_eq!(result, Duration::ZERO);
    }

    #[test]
    fn empty_claim_addresses_means_disabled() {
        let clock = fixed_clock(0);
        let mgr = BondManager::new(
            vec![],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        assert!(!mgr.is_enabled());
    }

    #[test]
    fn non_empty_claim_addresses_means_enabled() {
        let clock = fixed_clock(0);
        let mgr = BondManager::new(
            vec![Address::repeat_byte(0x01)],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        assert!(mgr.is_enabled());
    }

    // ---- discover_claimable_games tests ----

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
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        (factory, verifier)
    }

    #[tokio::test]
    async fn discover_incremental_picks_up_new_games_by_recipient() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(3, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
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

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 2);
    }

    #[tokio::test]
    async fn discover_skips_already_tracked_games() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(2, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
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
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0, "claimed game should not be tracked");
    }

    #[tokio::test]
    async fn discover_advances_watermark() {
        let claim_addr = Address::repeat_byte(0xCC);
        let (factory, verifier) = discovery_mocks(5, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
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

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
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
        let clock = fixed_clock(1000);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            5, // lookback = 5
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        // Simulate that the previous scan already covered everything.
        mgr.bond_scan_head = 10;

        // Force the full rescan by backdating `last_full_scan` past the
        // discovery interval.
        mgr.last_full_scan = Duration::from_secs(1000).saturating_sub(TEST_DISCOVERY_INTERVAL);

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        // Full rescan should have reset watermark to 10 - 5 = 5
        // and then scanned indices 5..10, discovering 5 new games.
        assert_eq!(mgr.bond_scan_head, 10);
        assert_eq!(mgr.tracked_count(), 5);
    }

    #[tokio::test]
    async fn discover_disabled_when_no_claim_addresses() {
        let (_, verifier) = discovery_mocks(5, Address::repeat_byte(0xCC), Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
    }

    #[tokio::test]
    async fn discover_skips_unmatched_recipients() {
        let claim_addr = Address::repeat_byte(0xCC);
        let other = Address::repeat_byte(0xDD);
        // Neither bondRecipient nor zkProver match our claim address.
        let (factory, verifier) = discovery_mocks(3, other, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        // Watermark should still advance past the scanned range.
        assert_eq!(mgr.bond_scan_head, 3);
    }

    #[tokio::test]
    async fn determine_phase_returns_awaiting_delay_when_bond_unlocked() {
        // resolved_at = 1_999_999_900 → age = 2_000_000_000 - 1_999_999_900 = 100s
        // monotonic 500 - 100 = 400 → unlocked_at should be 400s.
        let game = addr(0);
        let mut state = mock_state(1, Address::ZERO, 100);
        state.resolved_at = 1_999_999_900;
        state.bond_unlocked = true;

        let mut verifier_games = HashMap::new();
        verifier_games.insert(game, state);
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let clock = fixed_clock(500);
        let phase = BondManager::determine_phase(&*verifier, game, &clock).await.unwrap().unwrap();
        assert!(
            matches!(phase, BondPhase::AwaitingDelay { unlocked_at } if unlocked_at == Duration::from_secs(400)),
            "expected AwaitingDelay {{ unlocked_at: 400s }}, got {phase:?}",
        );
    }

    #[tokio::test]
    async fn try_unlock_advances_when_already_unlocked() {
        let claim_addr = Address::repeat_byte(0xCC);
        let game = addr(0);

        // resolved_at = 1_999_999_800 → age = 2_000_000_000 - 1_999_999_800 = 200s
        // monotonic 500 - 200 = 300 → unlocked_at should be 300s.
        let mut state = mock_state(1, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.resolved_at = 1_999_999_800;
        state.bond_unlocked = true;

        let mut verifier_games = HashMap::new();
        verifier_games.insert(game, state);
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));

        let clock = fixed_clock(500);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));
        mgr.track_game(game, claim_addr);

        // MockBondTransactionSubmitter should NOT be called — the bond is
        // already unlocked, so try_unlock must skip the transaction.
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        let result = mgr.try_unlock(game, &*verifier, &submitter).await.unwrap();
        assert!(result.is_none(), "try_unlock should not remove the game");

        let tracked = mgr.tracked.get(&game).expect("game should still be tracked");
        assert!(
            matches!(tracked.phase, BondPhase::AwaitingDelay { unlocked_at } if unlocked_at == Duration::from_secs(300)),
            "expected AwaitingDelay {{ unlocked_at: 300s }}, got {:?}",
            tracked.phase,
        );
        assert!(submitter.recorded_calls().is_empty(), "no transaction should have been submitted");
    }

    #[tokio::test]
    async fn discover_handles_empty_factory() {
        let claim_addr = Address::repeat_byte(0xCC);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            empty_factory(),
            1000,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );

        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.tracked_count(), 0);
        assert_eq!(mgr.bond_scan_head, 0);
    }

    // ---- lookback capping tests ----

    #[tokio::test]
    async fn discover_caps_span_to_lookback() {
        let claim_addr = Address::repeat_byte(0xCC);
        let lookback = 500u64;
        let game_count = 1200u64;
        let (factory, verifier) = discovery_mocks(game_count, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            lookback,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        // bond_scan_head starts at 0, game_count = 1200, span = 1200 > lookback.
        mgr.discover_claimable_games(&*verifier).await.unwrap();

        assert_eq!(
            mgr.bond_scan_head, lookback,
            "watermark should advance by lookback, not to game_count"
        );
        // Only games 0..500 should be discovered.
        assert_eq!(mgr.tracked_count(), lookback as usize);
    }

    #[tokio::test]
    async fn discover_catches_up_over_multiple_ticks() {
        let claim_addr = Address::repeat_byte(0xCC);
        let lookback = 500u64;
        let game_count = 800u64;
        let (factory, verifier) = discovery_mocks(game_count, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            lookback,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        // First tick: scans 0..500, watermark = 500.
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.bond_scan_head, lookback);
        assert_eq!(mgr.tracked_count(), lookback as usize);

        // Second tick: scans 500..800 (span = 300 < lookback), watermark = 800.
        mgr.discover_claimable_games(&*verifier).await.unwrap();
        assert_eq!(mgr.bond_scan_head, game_count);
        assert_eq!(mgr.tracked_count(), game_count as usize);
    }

    #[tokio::test]
    async fn discover_no_cap_when_span_within_lookback() {
        let claim_addr = Address::repeat_byte(0xCC);
        let lookback = 1000u64;
        let game_count = 500u64;
        let (factory, verifier) = discovery_mocks(game_count, claim_addr, Address::ZERO);

        let clock = fixed_clock(0);
        let mut mgr = BondManager::new(
            vec![claim_addr],
            test_l1_rpc_url(),
            factory,
            lookback,
            TEST_DISCOVERY_INTERVAL,
            clock,
        );
        mgr.set_weth_delay(Duration::from_secs(60));

        mgr.discover_claimable_games(&*verifier).await.unwrap();

        assert_eq!(mgr.bond_scan_head, game_count);
        assert_eq!(mgr.tracked_count(), game_count as usize);
    }

    // ---- anchor state update tests ----

    #[tokio::test]
    async fn anchor_update_skipped_for_needs_resolve_phase() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        // Game is still in NeedsResolve — should not attempt anchor update.

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty());
        assert!(!mgr.tracked.get(&game).unwrap().anchor_update_complete);
    }

    #[tokio::test]
    async fn anchor_update_skipped_for_challenger_wins() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::NeedsUnlock);

        // Status 1 = CHALLENGER_WINS — not eligible for anchor update.
        let mut state = mock_state(1, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;

        // No tx submitted, but marked complete (won't retry).
        assert!(submitter.recorded_calls().is_empty());
        assert!(mgr.tracked.get(&game).unwrap().anchor_update_complete);
    }

    #[tokio::test]
    async fn anchor_update_sends_tx_for_defender_wins() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::NeedsUnlock);

        // Status 2 = DEFENDER_WINS — eligible.
        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        let tx_hash = B256::repeat_byte(0xDD);
        let submitter = MockBondTransactionSubmitter::success(tx_hash);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;

        let calls = submitter.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, game, "tx should carry game address as context");
        assert_eq!(calls[0].1, asr, "tx should be sent to ASR address");
        assert!(mgr.tracked.get(&game).unwrap().anchor_update_complete);
    }

    #[tokio::test]
    async fn anchor_update_retries_on_failure() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::AwaitingDelay { unlocked_at: Duration::from_secs(0) });

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        // First attempt fails (e.g. airgap not elapsed → tx reverted).
        let submitter = MockBondTransactionSubmitter::with_responses(vec![Err(
            crate::ChallengeSubmitError::TxReverted { tx_hash: B256::ZERO },
        )]);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;

        // Should NOT be marked complete — will retry next tick.
        assert!(!mgr.tracked.get(&game).unwrap().anchor_update_complete);
    }

    #[tokio::test]
    async fn anchor_update_not_retried_after_success() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::NeedsUnlock);

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        // First call succeeds.
        let tx_hash = B256::repeat_byte(0xDD);
        let submitter = MockBondTransactionSubmitter::success(tx_hash);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;
        assert!(mgr.tracked.get(&game).unwrap().anchor_update_complete);

        // Second call should be a no-op (no submitter response needed).
        let submitter2 = MockBondTransactionSubmitter::with_responses(vec![]);
        mgr.try_anchor_update(game, &*verifier, &submitter2).await;
        assert!(submitter2.recorded_calls().is_empty());
    }

    async fn run_anchor_update_skip_case(
        mutate: impl FnOnce(&mut MockGameState),
        expect_complete: bool,
    ) {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::NeedsUnlock);

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        mutate(&mut state);
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        mgr.try_anchor_update(game, &*verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty(), "no tx expected");
        assert_eq!(
            mgr.tracked.get(&game).unwrap().anchor_update_complete,
            expect_complete,
            "anchor_update_complete mismatch"
        );
    }

    #[tokio::test]
    async fn anchor_update_skipped_for_blacklisted_game() {
        run_anchor_update_skip_case(|s| s.is_blacklisted = true, true).await;
    }

    #[tokio::test]
    async fn anchor_update_skipped_for_retired_game() {
        run_anchor_update_skip_case(|s| s.is_retired = true, true).await;
    }

    #[tokio::test]
    async fn anchor_update_retries_for_unrespected_game() {
        run_anchor_update_skip_case(|s| s.is_respected = false, false).await;
    }

    #[tokio::test]
    async fn anchor_update_skipped_for_stale_game() {
        run_anchor_update_skip_case(|s| s.anchor_root.l2_block_number = 200, true).await;
    }

    #[tokio::test]
    async fn anchor_update_waits_for_finalization() {
        run_anchor_update_skip_case(|s| s.is_finalized = false, false).await;
    }

    #[tokio::test]
    async fn poll_keeps_game_until_anchor_update_completes() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::Completed);

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        state.is_finalized = false;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));

        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);
        mgr.poll(&*verifier, &submitter).await;

        assert!(submitter.recorded_calls().is_empty(), "no tx should be sent before finalization");
        assert!(mgr.is_tracking(&game), "game should stay tracked until anchor update completes");
    }

    #[tokio::test]
    async fn poll_evicts_game_after_anchor_update_retention_timeout() {
        let claim_addr = Address::repeat_byte(0xCC);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.set_anchor_update_retention(Duration::from_secs(10));
        mgr.track_game(game, claim_addr);
        mgr.set_phase(game, BondPhase::Completed);

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = claim_addr;
        state.anchor_state_registry = asr;
        state.is_finalized = false;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        mgr.poll(&*verifier, &submitter).await;
        assert!(mgr.is_tracking(&game), "game should be retained before timeout");

        mgr.clock.monotonic = Duration::from_secs(10);
        mgr.poll(&*verifier, &submitter).await;
        assert!(!mgr.is_tracking(&game), "game should be evicted at retention timeout");
    }

    #[tokio::test]
    async fn poll_skips_bond_lifecycle_for_retained_not_claimable_game() {
        let claim_addr = Address::repeat_byte(0xCC);
        let other_recipient = Address::repeat_byte(0xDD);
        let asr = Address::repeat_byte(0xAA);
        let game = addr(0);

        let mut mgr = make_manager(vec![claim_addr]);
        mgr.track_game(game, claim_addr);

        let mut state = mock_state(2, Address::ZERO, 100);
        state.bond_recipient = other_recipient;
        state.anchor_state_registry = asr;
        state.is_finalized = false;
        let verifier = Arc::new(MockAggregateVerifier::new([(game, state)].into_iter().collect()));
        let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

        mgr.poll(&*verifier, &submitter).await;
        assert!(mgr.is_tracking(&game), "game should be retained for anchor update");
        assert_eq!(verifier.bond_recipient_read_count(game), 1);

        mgr.poll(&*verifier, &submitter).await;
        assert!(mgr.is_tracking(&game), "game should remain retained before finalization");
        assert_eq!(
            verifier.bond_recipient_read_count(game),
            1,
            "retained game should not re-run the bond lifecycle"
        );
    }
}
