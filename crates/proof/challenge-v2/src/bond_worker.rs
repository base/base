//! Per-bond worker: chains the resolve / unlock / wait / withdraw /
//! close-game steps for a single dispute game's bond.
//!
//! Each step reads the live on-chain state before acting, so a fresh
//! worker started after a process restart skips any step that has
//! already been completed.

use std::{
    collections::HashSet,
    future::Future,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::Address;
use base_proof_contracts::{AggregateVerifierClient, ContractError, DelayedWETHClient, GameStatus};
use derive_more::Debug;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{BondAction, BondCandidate, BondRequest, Submission};

/// Read-only handles, channels and config shared across every
/// bond-worker task.
#[derive(Debug)]
pub struct BondWorkerDeps {
    /// Aggregate verifier read for status, recipient, and unlock /
    /// claimed flags.
    #[debug(skip)]
    pub verifier: Arc<dyn AggregateVerifierClient>,
    /// `DelayedWETH` client read for the unlock timestamp and delay.
    #[debug(skip)]
    pub weth: Arc<dyn DelayedWETHClient>,
    /// Outbound channel to the [`crate::SubmissionTask`].
    #[debug(skip)]
    pub submission_tx: mpsc::Sender<Submission>,
    /// Recipient addresses the worker is willing to spend gas for.
    pub claim_addresses: HashSet<Address>,
    /// Static worker config.
    pub config: BondWorkerConfig,
}

impl BondWorkerDeps {
    /// Bundles the read clients, channel and config for sharing across workers.
    pub fn new(
        verifier: Arc<dyn AggregateVerifierClient>,
        weth: Arc<dyn DelayedWETHClient>,
        submission_tx: mpsc::Sender<Submission>,
        claim_addresses: HashSet<Address>,
        config: BondWorkerConfig,
    ) -> Self {
        Self { verifier, weth, submission_tx, claim_addresses, config }
    }
}

/// Per-bond-worker configuration. `Copy` so it flows through async
/// boundaries without atomics or clones.
#[derive(Debug, Clone, Copy)]
pub struct BondWorkerConfig {
    /// Sleep between successive on-chain polls while waiting for a
    /// submitted transaction to take effect.
    pub tx_confirm_poll_interval: Duration,
    /// Maximum total time to wait for any single submitted transaction
    /// to take effect on-chain.
    pub tx_confirm_timeout: Duration,
}

/// Failure reported by [`run_bond_worker`].
#[derive(Debug, Error)]
pub enum BondError {
    /// A read against `AggregateVerifier` or `DelayedWETH` failed.
    #[error("contract call failed: {0}")]
    Contract(#[from] ContractError),
    /// The submission channel was closed before the worker could
    /// dispatch its next bond request.
    #[error("submission channel closed")]
    SubmissionChannelClosed,
    /// The cancellation token fired during a sleep or poll.
    #[error("worker cancelled")]
    Cancelled,
    /// A submitted transaction did not take effect within
    /// [`BondWorkerConfig::tx_confirm_timeout`].
    #[error("step `{0}` did not confirm in time")]
    Timeout(&'static str),
}

/// Runs the full bond-claim pipeline for `candidate` and exits.
///
/// Each step is idempotent: a fresh worker started after a restart
/// observes the on-chain state and skips any step that already
/// completed. Errors are logged and end the worker; the next
/// [`crate::BondDiscovery`] tick re-emits the candidate and a new
/// worker resumes from wherever the on-chain state currently sits.
pub async fn run_bond_worker(
    candidate: BondCandidate,
    deps: Arc<BondWorkerDeps>,
    cancel: CancellationToken,
) {
    let game = candidate.game_address;
    match run_pipeline(candidate, &deps, &cancel).await {
        Ok(()) => {}
        Err(BondError::Cancelled) => debug!(%game, "bond worker cancelled"),
        Err(e) => warn!(%game, error = %e, "bond worker failed"),
    }
}

/// The pipeline in `?`-friendly form so [`run_bond_worker`] can map
/// the error variants once at the top. Returns `Ok(())` both for full
/// completion and for benign early exits (game not resolvable yet,
/// recipient outside our claim set), each logged at the exit site.
async fn run_pipeline(
    candidate: BondCandidate,
    deps: &BondWorkerDeps,
    cancel: &CancellationToken,
) -> Result<(), BondError> {
    let game = candidate.game_address;

    if !ensure_resolved(game, deps, cancel).await? {
        debug!(%game, "game not yet resolvable; exiting until next discovery tick");
        return Ok(());
    }

    // After resolve, `bondRecipient` may have been overwritten to a
    // winner outside our claim set; verify before spending more gas.
    let recipient = deps.verifier.bond_recipient(game).await?;
    if !deps.claim_addresses.contains(&recipient) {
        info!(%game, %recipient, "bond recipient outside claim set; exiting");
        return Ok(());
    }

    ensure_unlocked(game, deps, cancel).await?;
    wait_weth_delay(game, recipient, current_unix_secs(), deps, cancel).await?;
    ensure_withdrawn(game, deps, cancel).await?;
    close_game(game, deps).await?;

    info!(%game, "bond claim pipeline complete");
    Ok(())
}

/// Returns `true` once the game is in a resolved state, submitting
/// `resolve()` first if needed. Returns `false` when the game cannot
/// yet be resolved (`gameOver()` is `false`), letting the caller exit
/// and rely on the next discovery tick to re-spawn a worker.
async fn ensure_resolved(
    game: Address,
    deps: &BondWorkerDeps,
    cancel: &CancellationToken,
) -> Result<bool, BondError> {
    if is_resolved(deps.verifier.status(game).await?) {
        return Ok(true);
    }

    if !deps.verifier.game_over(game).await? {
        return Ok(false);
    }

    submit(deps, game, BondAction::Resolve).await?;
    poll_until(
        "ensure_resolved",
        || async { Ok(is_resolved(deps.verifier.status(game).await?)) },
        deps.config,
        cancel,
    )
    .await?;

    Ok(true)
}

/// Submits the first `claimCredit()` (unlock) and polls until
/// `bondUnlocked()` is `true`.
async fn ensure_unlocked(
    game: Address,
    deps: &BondWorkerDeps,
    cancel: &CancellationToken,
) -> Result<(), BondError> {
    if deps.verifier.bond_unlocked(game).await? {
        return Ok(());
    }

    submit(deps, game, BondAction::UnlockCredit).await?;
    poll_until(
        "ensure_unlocked",
        || async { deps.verifier.bond_unlocked(game).await },
        deps.config,
        cancel,
    )
    .await
}

/// Sleeps for the remaining time between the recorded `unlock` and
/// the moment `withdraw` becomes callable. No-op when the delay has
/// already elapsed.
async fn wait_weth_delay(
    game: Address,
    recipient: Address,
    now_secs: u64,
    deps: &BondWorkerDeps,
    cancel: &CancellationToken,
) -> Result<(), BondError> {
    // `ensure_unlocked` only returns after `bondUnlocked == true`, and
    // `claimCredit()` atomically calls `DELAYED_WETH.unlock` (which
    // sets `timestamp = block.timestamp`, always non-zero) before
    // flipping the flag. We can therefore trust `unlock_ts > 0` here.
    let (_, unlock_ts) = deps.weth.withdrawals(game, recipient).await?;
    let delay = deps.weth.delay().await?;
    let ready_at = unlock_ts.saturating_add(delay.as_secs());
    if now_secs >= ready_at {
        return Ok(());
    }

    let remaining = Duration::from_secs(ready_at - now_secs);
    sleep_cancellable(remaining, cancel).await
}

/// Submits the second `claimCredit()` (withdraw) and polls until
/// `bondClaimed()` is `true`.
async fn ensure_withdrawn(
    game: Address,
    deps: &BondWorkerDeps,
    cancel: &CancellationToken,
) -> Result<(), BondError> {
    if deps.verifier.bond_claimed(game).await? {
        return Ok(());
    }

    submit(deps, game, BondAction::WithdrawCredit).await?;
    poll_until(
        "ensure_withdrawn",
        || async { deps.verifier.bond_claimed(game).await },
        deps.config,
        cancel,
    )
    .await
}

/// Submits `closeGame()`. Best-effort: confirmation is left to the
/// next discovery tick or the next proposer's preflight.
async fn close_game(game: Address, deps: &BondWorkerDeps) -> Result<(), BondError> {
    submit(deps, game, BondAction::CloseGame).await
}

/// Sends a [`BondRequest`] through the submission channel.
async fn submit(deps: &BondWorkerDeps, game: Address, action: BondAction) -> Result<(), BondError> {
    let request = BondRequest { game_address: game, action };
    deps.submission_tx
        .send(Submission::Bond(request))
        .await
        .map_err(|_| BondError::SubmissionChannelClosed)
}

/// Polls `predicate` every `config.tx_confirm_poll_interval` until it
/// returns `true` or `config.tx_confirm_timeout` elapses, with `cancel`
/// preempting any sleep.
async fn poll_until<F, Fut>(
    label: &'static str,
    mut predicate: F,
    config: BondWorkerConfig,
    cancel: &CancellationToken,
) -> Result<(), BondError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool, ContractError>>,
{
    let deadline = Instant::now() + config.tx_confirm_timeout;
    loop {
        sleep_cancellable(config.tx_confirm_poll_interval, cancel).await?;
        if predicate().await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BondError::Timeout(label));
        }
    }
}

/// Sleeps `duration` or returns [`BondError::Cancelled`] if `cancel`
/// fires first.
async fn sleep_cancellable(
    duration: Duration,
    cancel: &CancellationToken,
) -> Result<(), BondError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(BondError::Cancelled),
        () = tokio::time::sleep(duration) => Ok(()),
    }
}

/// Returns `true` for [`GameStatus::ChallengerWins`] or [`GameStatus::DefenderWins`].
const fn is_resolved(status: GameStatus) -> bool {
    matches!(status, GameStatus::ChallengerWins | GameStatus::DefenderWins)
}

/// Wall-clock seconds since the Unix epoch.
fn current_unix_secs() -> u64 {
    SystemTime::UNIX_EPOCH.elapsed().expect("system clock is before UNIX_EPOCH").as_secs()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};

    use super::*;
    use crate::test_utils::{MockAggregateVerifier, MockDelayedWETH, MockGameState};

    const GAME: Address = address!("00000000000000000000000000000000000000a1");
    const RECIPIENT: Address = address!("00000000000000000000000000000000000000b2");
    const OUTSIDER: Address = address!("00000000000000000000000000000000000000ee");
    const WETH_DELAY_SECS: u64 = 7 * 24 * 60 * 60;
    const POLL_INTERVAL_MS: u64 = 10;
    const TX_CONFIRM_TIMEOUT_MS: u64 = 200;

    /// Bundles the mocks plus a wired `BondWorkerDeps` so each test
    /// can program one or two fields without re-wiring the channel.
    struct Fixture {
        verifier: Arc<MockAggregateVerifier>,
        weth: Arc<MockDelayedWETH>,
        submission_rx: mpsc::Receiver<Submission>,
        deps: Arc<BondWorkerDeps>,
        cancel: CancellationToken,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_claim_addresses([RECIPIENT])
        }

        fn with_claim_addresses(addrs: impl IntoIterator<Item = Address>) -> Self {
            let verifier = Arc::new(MockAggregateVerifier::new());
            let weth = Arc::new(MockDelayedWETH::new(Duration::from_secs(WETH_DELAY_SECS)));
            let (tx, rx) = mpsc::channel(8);
            let claim = addrs.into_iter().collect::<HashSet<_>>();
            let config = BondWorkerConfig {
                tx_confirm_poll_interval: Duration::from_millis(POLL_INTERVAL_MS),
                tx_confirm_timeout: Duration::from_millis(TX_CONFIRM_TIMEOUT_MS),
            };
            let deps = Arc::new(BondWorkerDeps::new(
                Arc::<MockAggregateVerifier>::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
                Arc::<MockDelayedWETH>::clone(&weth) as Arc<dyn DelayedWETHClient>,
                tx,
                claim,
                config,
            ));
            Self { verifier, weth, submission_rx: rx, deps, cancel: CancellationToken::new() }
        }

        fn set_state(&self, state: MockGameState) {
            self.verifier.set_game(GAME, state);
        }
    }

    fn in_progress(game_over: bool) -> MockGameState {
        let mut s = MockGameState::in_progress(Address::ZERO, Address::ZERO, 0);
        s.game_over = game_over;
        s.bond_recipient = RECIPIENT;
        s
    }

    fn resolved(recipient: Address) -> MockGameState {
        let mut s = MockGameState::in_progress(Address::ZERO, Address::ZERO, 0);
        s.status = GameStatus::ChallengerWins;
        s.bond_recipient = recipient;
        s
    }

    fn expect_bond_action(submission: Submission, expected: BondAction) {
        let Submission::Bond(req) = submission else {
            panic!("expected Submission::Bond, got {submission:?}");
        };
        assert_eq!(req.game_address, GAME);
        assert_eq!(req.action, expected);
    }

    // ── ensure_resolved ────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_resolved_skips_when_already_resolved() {
        let mut fx = Fixture::new();
        fx.set_state(resolved(RECIPIENT));

        let result = ensure_resolved(GAME, &fx.deps, &fx.cancel).await;

        assert!(result.unwrap());
        assert!(fx.submission_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ensure_resolved_returns_false_when_not_game_over() {
        let mut fx = Fixture::new();
        fx.set_state(in_progress(false));

        let result = ensure_resolved(GAME, &fx.deps, &fx.cancel).await;

        assert!(!result.unwrap());
        assert!(fx.submission_rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_resolved_submits_and_polls_until_status_changes() {
        let fx = Fixture::new();
        fx.set_state(in_progress(true));
        let verifier = Arc::clone(&fx.verifier);
        let mut submission_rx = fx.submission_rx;

        let drive = async {
            let submission = submission_rx.recv().await.expect("submission must arrive");
            expect_bond_action(submission, BondAction::Resolve);
            verifier.set_game(GAME, resolved(RECIPIENT));
            tokio::time::advance(Duration::from_millis(POLL_INTERVAL_MS + 1)).await;
        };
        let (result, ()) = tokio::join!(ensure_resolved(GAME, &fx.deps, &fx.cancel), drive);

        assert!(result.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_resolved_times_out_when_status_never_changes() {
        let fx = Fixture::new();
        fx.set_state(in_progress(true));
        let mut submission_rx = fx.submission_rx;

        let drive = async {
            // Drain the submission so the channel doesn't block.
            let _ = submission_rx.recv().await;
            tokio::time::advance(Duration::from_millis(TX_CONFIRM_TIMEOUT_MS + 50)).await;
        };
        let (result, ()) = tokio::join!(ensure_resolved(GAME, &fx.deps, &fx.cancel), drive);

        assert!(matches!(result, Err(BondError::Timeout("ensure_resolved"))));
    }

    // ── ensure_unlocked ─────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_unlocked_skips_when_already_unlocked() {
        let mut fx = Fixture::new();
        let mut state = resolved(RECIPIENT);
        state.bond_unlocked = true;
        fx.set_state(state);

        let result = ensure_unlocked(GAME, &fx.deps, &fx.cancel).await;

        assert!(result.is_ok());
        assert!(fx.submission_rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_unlocked_submits_and_polls_until_unlocked() {
        let fx = Fixture::new();
        fx.set_state(resolved(RECIPIENT));
        let verifier = Arc::clone(&fx.verifier);
        let mut submission_rx = fx.submission_rx;

        let drive = async {
            let submission = submission_rx.recv().await.expect("submission must arrive");
            expect_bond_action(submission, BondAction::UnlockCredit);
            let mut state = resolved(RECIPIENT);
            state.bond_unlocked = true;
            verifier.set_game(GAME, state);
            tokio::time::advance(Duration::from_millis(POLL_INTERVAL_MS + 1)).await;
        };
        let (result, ()) = tokio::join!(ensure_unlocked(GAME, &fx.deps, &fx.cancel), drive);

        assert!(result.is_ok());
    }

    // ── wait_weth_delay ─────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_weth_delay_returns_immediately_when_already_elapsed() {
        let fx = Fixture::new();
        let unlock_ts = 1_000_000;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let now = unlock_ts + WETH_DELAY_SECS + 1;
        let result = wait_weth_delay(GAME, RECIPIENT, now, &fx.deps, &fx.cancel).await;

        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_weth_delay_sleeps_for_remaining_window() {
        let fx = Fixture::new();
        let unlock_ts = 1_000_000;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let now = unlock_ts + 100;
        let remaining = WETH_DELAY_SECS - 100;
        let task = tokio::spawn({
            let deps = Arc::clone(&fx.deps);
            let cancel = fx.cancel.clone();
            async move { wait_weth_delay(GAME, RECIPIENT, now, &deps, &cancel).await }
        });

        // One second short of ready: still parked.
        tokio::time::advance(Duration::from_secs(remaining - 1)).await;
        assert!(!task.is_finished());

        // Cross the boundary: completes.
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = task.await.expect("task must not panic");
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_weth_delay_returns_cancelled_when_token_fires() {
        let fx = Fixture::new();
        let unlock_ts = 1_000_000;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let cancel = fx.cancel.clone();
        let task = tokio::spawn({
            let deps = Arc::clone(&fx.deps);
            async move { wait_weth_delay(GAME, RECIPIENT, unlock_ts + 100, &deps, &cancel).await }
        });

        // Yield once so the task enters the sleep before cancel fires.
        tokio::task::yield_now().await;
        fx.cancel.cancel();
        let result = task.await.expect("task must not panic");
        assert!(matches!(result, Err(BondError::Cancelled)));
    }

    // ── ensure_withdrawn ────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_withdrawn_skips_when_already_claimed() {
        let mut fx = Fixture::new();
        let mut state = resolved(RECIPIENT);
        state.bond_unlocked = true;
        state.bond_claimed = true;
        fx.set_state(state);

        let result = ensure_withdrawn(GAME, &fx.deps, &fx.cancel).await;

        assert!(result.is_ok());
        assert!(fx.submission_rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_withdrawn_submits_and_polls_until_claimed() {
        let fx = Fixture::new();
        let mut state = resolved(RECIPIENT);
        state.bond_unlocked = true;
        fx.set_state(state);
        let verifier = Arc::clone(&fx.verifier);
        let mut submission_rx = fx.submission_rx;

        let drive = async {
            let submission = submission_rx.recv().await.expect("submission must arrive");
            expect_bond_action(submission, BondAction::WithdrawCredit);
            let mut state = resolved(RECIPIENT);
            state.bond_unlocked = true;
            state.bond_claimed = true;
            verifier.set_game(GAME, state);
            tokio::time::advance(Duration::from_millis(POLL_INTERVAL_MS + 1)).await;
        };
        let (result, ()) = tokio::join!(ensure_withdrawn(GAME, &fx.deps, &fx.cancel), drive);

        assert!(result.is_ok());
    }

    // ── close_game ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn close_game_submits_close_game_action() {
        let mut fx = Fixture::new();

        let result = close_game(GAME, &fx.deps).await;

        assert!(result.is_ok());
        let submission = fx.submission_rx.try_recv().expect("submission must arrive");
        expect_bond_action(submission, BondAction::CloseGame);
    }

    // ── run_bond_worker (integration) ───────────────────────────────────

    #[tokio::test]
    async fn pipeline_exits_when_recipient_outside_claim_set() {
        let fx = Fixture::with_claim_addresses([RECIPIENT]);
        fx.set_state(resolved(OUTSIDER));
        let mut submission_rx = fx.submission_rx;

        run_bond_worker(
            BondCandidate { game_address: GAME, bond_recipient: RECIPIENT },
            Arc::clone(&fx.deps),
            fx.cancel.clone(),
        )
        .await;

        // No submission was sent (we exited before unlock).
        assert!(submission_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn pipeline_exits_when_game_not_resolvable_yet() {
        let fx = Fixture::new();
        fx.set_state(in_progress(false));
        let mut submission_rx = fx.submission_rx;

        run_bond_worker(
            BondCandidate { game_address: GAME, bond_recipient: RECIPIENT },
            Arc::clone(&fx.deps),
            fx.cancel.clone(),
        )
        .await;

        // No submission: game not yet resolvable.
        assert!(submission_rx.try_recv().is_err());
    }
}
