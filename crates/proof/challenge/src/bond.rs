//! Bond lifecycle management for the challenger.
//!
//! After the challenger successfully submits a `challenge()` transaction,
//! the [`BondManager`] tracks the resulting game through a multi-phase
//! credit claim lifecycle:
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
//! `BASE_CHALLENGER_BOND_CLAIM_ADDRESSES` env var. The manager only tracks
//! games where the on-chain `bondRecipient` or `zkProver` matches one of
//! those addresses. Checking `zkProver` is necessary to recover pre-resolve
//! challenged games after a restart, since `bondRecipient` is only updated
//! to the challenger's address during `resolve()`.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime},
};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, encode_claim_credit_calldata, encode_resolve_calldata,
};
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{ChallengerMetrics, GameScanner};

/// Returns the current Unix timestamp in seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
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
        /// Unix timestamp (seconds) at which the unlock occurred.
        unlocked_at: u64,
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
/// on-chain state and submits the next transaction in the lifecycle.
#[derive(Debug)]
pub struct BondManager {
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
}

impl BondManager {
    /// Conservative fallback when the on-chain `DelayedWETH` delay has not
    /// been read yet. If the real delay is shorter the withdraw will simply
    /// succeed earlier; if longer, the attempt reverts and is retried.
    const DEFAULT_WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Creates a new bond manager for the given set of claim addresses.
    pub fn new(claim_addresses: Vec<Address>, l1_rpc_url: url::Url) -> Self {
        let set: HashSet<Address> = claim_addresses.into_iter().collect();
        info!(count = set.len(), "bond manager initialized with claim addresses");
        Self { tracked: HashMap::new(), claim_addresses: set, weth_delay: None, l1_rpc_url }
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
    /// delay has not been set yet.
    pub async fn startup_scan(
        &mut self,
        factory_client: &dyn DisputeGameFactoryClient,
        verifier_client: &dyn AggregateVerifierClient,
        lookback: u64,
    ) -> eyre::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let game_count = factory_client.game_count().await?;
        if game_count == 0 {
            info!("no games in factory, skipping bond startup scan");
            return Ok(());
        }

        let start_index = game_count.saturating_sub(lookback);
        info!(start = start_index, end = game_count, "scanning recent games for bond recovery");

        // Evaluate all games concurrently using the same pattern as the
        // game scanner (`buffer_unordered`).
        let claim_addresses = &self.claim_addresses;
        let results: Vec<Option<(Address, Address, Option<BondPhase>)>> =
            stream::iter(start_index..game_count)
                .map(|i| async move {
                    let game_at = match factory_client.game_at_index(i).await {
                        Ok(g) => g,
                        Err(e) => {
                            warn!(index = i, error = %e, "failed to fetch game at index");
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
                            return None;
                        }
                    };

                    // Check both `bondRecipient` and `zkProver` against our
                    // claim addresses. Before `resolve()`, `bondRecipient`
                    // is the game creator while `zkProver` is the address
                    // that called `challenge()`. After `resolve()`,
                    // `bondRecipient` is updated to the `zkProver`. Checking
                    // both ensures we recover pre-resolve challenged games
                    // after a restart.
                    let matched_address = if claim_addresses.contains(&bond_recipient) {
                        bond_recipient
                    } else if zk_prover != Address::ZERO && claim_addresses.contains(&zk_prover) {
                        zk_prover
                    } else {
                        return None;
                    };

                    let phase = match Self::determine_phase(verifier_client, game_address).await {
                        Ok(phase) => phase,
                        Err(e) => {
                            warn!(
                                game = %game_address,
                                error = %e,
                                "failed to determine bond phase"
                            );
                            return None;
                        }
                    };

                    Some((game_address, matched_address, phase))
                })
                .buffer_unordered(GameScanner::SCAN_CONCURRENCY)
                .collect()
                .await;

        // Process results sequentially: insert tracked games and resolve the
        // WETH delay from the first relevant game.
        let mut weth_delay_resolved = self.weth_delay.is_some();

        for (game_address, bond_recipient, phase) in results.into_iter().flatten() {
            // Read the WETH delay from the first relevant game if not yet set.
            if !weth_delay_resolved {
                if let Err(e) = self.resolve_weth_delay(verifier_client, game_address).await {
                    warn!(error = %e, "failed to read DelayedWETH delay, will retry later");
                } else {
                    weth_delay_resolved = true;
                }
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

        ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        info!(count = self.tracked.len(), "bond startup scan complete");
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
        let mut completed = Vec::new();

        for game_address in addresses {
            match self.advance_game(game_address, verifier_client, submitter).await {
                Ok(true) => completed.push(game_address),
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

        for addr in &completed {
            self.tracked.remove(addr);
            ChallengerMetrics::bonds_completed_total().increment(1);
        }

        if !completed.is_empty() {
            ChallengerMetrics::bonds_tracked().set(self.tracked.len() as f64);
        }
    }

    /// Advances a single game through the bond lifecycle state machine.
    ///
    /// Returns `Ok(true)` when the game reaches [`BondPhase::Completed`] and
    /// should be removed from tracking.
    async fn advance_game(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<bool> {
        let game = match self.tracked.get(&game_address) {
            Some(g) => g,
            None => return Ok(false),
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
            BondPhase::Completed => Ok(true),
        }
    }

    /// Marks a game as completed because the defender won and the bond is not
    /// claimable. Returns `Ok(true)` to signal removal from tracking.
    fn complete_defender_wins(&mut self, game_address: Address, status: u8) -> eyre::Result<bool> {
        info!(
            game = %game_address,
            status,
            "game resolved as DEFENDER_WINS, bond not claimable — removing from tracking"
        );
        ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_DEFENDER_WINS)
            .increment(1);
        self.set_phase(game_address, BondPhase::Completed);
        Ok(true)
    }

    /// Attempts to resolve the game by calling `resolve()`.
    ///
    /// After resolution (either by us or by another actor), checks the
    /// on-chain status to verify the game resolved as `CHALLENGER_WINS`.
    /// If the game resolved as `DEFENDER_WINS`, the bond belongs to the
    /// game creator and is not claimable by the challenger, so the game
    /// is removed from tracking.
    async fn try_resolve(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<bool> {
        // Check if already resolved on-chain (e.g., someone else called it).
        let status = verifier_client.status(game_address).await?;
        if status != GameScanner::STATUS_IN_PROGRESS {
            ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ALREADY_RESOLVED)
                .increment(1);

            if status != GameScanner::STATUS_CHALLENGER_WINS {
                return self.complete_defender_wins(game_address, status);
            }

            info!(game = %game_address, status, "game already resolved, advancing to unlock phase");
            self.set_phase(game_address, BondPhase::NeedsUnlock);
            return Ok(false);
        }

        // Check if the game is ready to resolve.
        let game_over = verifier_client.game_over(game_address).await?;
        if !game_over {
            debug!(game = %game_address, "game dispute period not yet elapsed");
            return Ok(false);
        }

        // Submit resolve transaction.
        let calldata = encode_resolve_calldata();
        match submitter.send_bond_tx(game_address, calldata).await {
            Ok(tx_hash) => {
                info!(game = %game_address, tx_hash = %tx_hash, "resolve transaction confirmed");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);

                // Re-read status to verify the game resolved in our favor.
                let post_status = verifier_client.status(game_address).await?;
                if post_status != GameScanner::STATUS_CHALLENGER_WINS {
                    return self.complete_defender_wins(game_address, post_status);
                }

                self.set_phase(game_address, BondPhase::NeedsUnlock);
            }
            Err(e) => {
                warn!(game = %game_address, error = %e, "resolve transaction failed, will retry");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
            }
        }
        Ok(false)
    }

    /// Attempts the first `claimCredit()` call to trigger the unlock.
    async fn try_unlock(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<bool> {
        // Check if already unlocked on-chain.
        let unlocked = verifier_client.bond_unlocked(game_address).await?;
        if unlocked {
            // Use `resolved_at` as a conservative lower bound for the unlock
            // time. The unlock must have occurred after resolve, so this may
            // cause one early withdrawal attempt that reverts, but is strictly
            // better than resetting to "now" (which would re-impose the full
            // delay after every restart).
            let resolved_at = verifier_client.resolved_at(game_address).await?;
            info!(
                game = %game_address,
                resolved_at,
                "bond already unlocked, advancing to delay phase"
            );
            self.set_phase(game_address, BondPhase::AwaitingDelay { unlocked_at: resolved_at });
            return Ok(false);
        }

        self.submit_claim_credit(
            game_address,
            submitter,
            "unlock",
            BondPhase::AwaitingDelay { unlocked_at: unix_now() },
        )
        .await
    }

    /// Checks if the `DelayedWETH` delay has elapsed since the unlock.
    fn check_delay(&mut self, game_address: Address, unlocked_at: u64) -> eyre::Result<bool> {
        // Fall back to 7 days if the on-chain delay has not been read yet.
        // If the real delay is shorter, the withdraw attempt will simply
        // succeed earlier than expected. If longer, the attempt will revert
        // and be retried on the next poll tick.
        let delay = self.weth_delay.unwrap_or_else(|| {
            warn!(game = %game_address, "WETH delay not yet known, using default 7 days");
            Self::DEFAULT_WETH_DELAY
        });

        let now = unix_now();
        let elapsed = Duration::from_secs(now.saturating_sub(unlocked_at));

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
        Ok(false)
    }

    /// Attempts the second `claimCredit()` call to complete the withdrawal.
    async fn try_withdraw(
        &mut self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &dyn BondTransactionSubmitter,
    ) -> eyre::Result<bool> {
        // Check if already claimed on-chain.
        let claimed = verifier_client.bond_claimed(game_address).await?;
        if claimed {
            info!(game = %game_address, "bond already claimed");
            self.set_phase(game_address, BondPhase::Completed);
            return Ok(true);
        }

        self.submit_claim_credit(game_address, submitter, "withdraw", BondPhase::Completed)
            .await
    }

    /// Submits a `claimCredit()` transaction and transitions to the given
    /// phase on success. Returns `Ok(true)` when the success phase is
    /// [`BondPhase::Completed`].
    async fn submit_claim_credit(
        &mut self,
        game_address: Address,
        submitter: &dyn BondTransactionSubmitter,
        step: &str,
        success_phase: BondPhase,
    ) -> eyre::Result<bool> {
        let calldata = encode_claim_credit_calldata();
        ChallengerMetrics::claim_credit_tx_submitted_total().increment(1);
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
                Ok(completed)
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
                Ok(false)
            }
        }
    }

    /// Determines the bond phase from on-chain state.
    ///
    /// Returns `None` if the bond has already been claimed, or if the game
    /// resolved as `DEFENDER_WINS` (the bond is not claimable by the
    /// challenger).
    async fn determine_phase(
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
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
            return Ok(Some(BondPhase::AwaitingDelay { unlocked_at: resolved_at }));
        }

        if resolved_at > 0 {
            // The game has been resolved. Check if it resolved in the
            // challenger's favor before tracking it for bond claiming.
            let status = verifier_client.status(game_address).await?;
            if status != GameScanner::STATUS_CHALLENGER_WINS {
                debug!(
                    game = %game_address,
                    status,
                    "game resolved as DEFENDER_WINS during startup scan, skipping"
                );
                return Ok(None);
            }
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
    use super::*;

    fn test_l1_rpc_url() -> url::Url {
        "http://localhost:8545".parse().unwrap()
    }

    fn make_manager(addresses: Vec<Address>) -> BondManager {
        let mut mgr = BondManager::new(addresses, test_l1_rpc_url());
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

        let mut mgr = BondManager::new(vec![addr], test_l1_rpc_url());
        mgr.set_weth_delay(Duration::from_secs(0)); // instant delay for testing
        let past = unix_now().saturating_sub(1);
        mgr.tracked.insert(
            game,
            TrackedGame {
                phase: BondPhase::AwaitingDelay { unlocked_at: past },
                bond_recipient: addr,
            },
        );

        let result = mgr.check_delay(game, past);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::NeedsWithdraw));
    }

    #[test]
    fn check_delay_stays_when_not_elapsed() {
        let addr = Address::repeat_byte(0x01);
        let game = Address::repeat_byte(0xAA);

        let mut mgr = BondManager::new(vec![addr], test_l1_rpc_url());
        mgr.set_weth_delay(Duration::from_secs(3600));
        let now = unix_now();
        mgr.tracked.insert(
            game,
            TrackedGame {
                phase: BondPhase::AwaitingDelay { unlocked_at: now },
                bond_recipient: addr,
            },
        );

        let result = mgr.check_delay(game, now);
        assert!(result.is_ok());
        assert!(matches!(mgr.tracked.get(&game).unwrap().phase, BondPhase::AwaitingDelay { .. }));
    }

    #[test]
    fn empty_claim_addresses_means_disabled() {
        let mgr = BondManager::new(vec![], test_l1_rpc_url());
        assert!(!mgr.is_enabled());
    }

    #[test]
    fn non_empty_claim_addresses_means_enabled() {
        let mgr = BondManager::new(vec![Address::repeat_byte(0x01)], test_l1_rpc_url());
        assert!(mgr.is_enabled());
    }
}
