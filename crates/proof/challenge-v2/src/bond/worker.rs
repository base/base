//! Per-bond worker: chains the resolve / unlock / wait / withdraw /
//! close-game steps for a single dispute game's bond.
//!
//! Each step reads the live on-chain state before acting, so a fresh
//! worker started after a process restart skips any step that has
//! already been completed.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy_primitives::Address;
use base_proof_contracts::{AggregateVerifierClient, ContractError, GameStatus};
use derive_more::Debug;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    BondAction, BondCandidate, BondRequest, DelayedWETHResolver, Submission, SubmissionHandle,
    SubmitError,
};

/// Shared deps for every bond-worker task.
#[derive(Debug)]
pub struct BondWorkerDeps {
    /// Aggregate verifier client.
    #[debug(skip)]
    pub verifier: Arc<dyn AggregateVerifierClient>,
    /// Per-game `DelayedWETH` client resolver.
    #[debug(skip)]
    pub delayed_weth_resolver: Arc<dyn DelayedWETHResolver>,
    /// Submission entry point.
    pub handle: SubmissionHandle,
    /// Recipient addresses the worker spends gas for.
    pub claim_addresses: HashSet<Address>,
}

impl BondWorkerDeps {
    /// Builds a [`BondWorkerDeps`].
    pub fn new(
        verifier: Arc<dyn AggregateVerifierClient>,
        delayed_weth_resolver: Arc<dyn DelayedWETHResolver>,
        handle: SubmissionHandle,
        claim_addresses: HashSet<Address>,
    ) -> Self {
        Self { verifier, delayed_weth_resolver, handle, claim_addresses }
    }
}

/// Failure reported by [`run_bond_worker`].
#[derive(Debug, Error)]
pub enum BondError {
    /// A read against `AggregateVerifier` or `DelayedWETH` failed.
    #[error("contract call failed: {0}")]
    Contract(#[from] ContractError),
    /// Submission failed and on-chain state did not reach the expected value.
    #[error("step `{step}` failed: {source}")]
    Submit {
        /// Pipeline step that issued the submission.
        step: &'static str,
        /// Underlying error.
        #[source]
        source: SubmitError,
    },
}

/// Runs the bond-claim pipeline for `candidate` and exits.
/// Each step is idempotent across restarts. `cancel` preempts the
/// pipeline at any await point.
pub async fn run_bond_worker(
    candidate: BondCandidate,
    deps: Arc<BondWorkerDeps>,
    cancel: CancellationToken,
) {
    let game = candidate.game_address;
    tokio::select! {
        biased;
        () = cancel.cancelled() => debug!(%game, "bond worker cancelled"),
        result = run_pipeline(candidate, &deps) => match result {
            Ok(()) => {}
            Err(e) => warn!(%game, error = %e, "bond worker failed"),
        }
    }
}

/// Runs the bond-claim pipeline for a single game, in order:
///
/// 1. resolve the game (or exit if not yet resolvable)
/// 2. check `bondRecipient` is in our claim set (or exit)
/// 3. submit `claimCredit()` (unlock)
/// 4. sleep through the WETH withdrawal delay
/// 5. submit `claimCredit()` (withdraw)
/// 6. submit `closeGame()` (best-effort)
async fn run_pipeline(candidate: BondCandidate, deps: &BondWorkerDeps) -> Result<(), BondError> {
    let game = candidate.game_address;

    if !ensure_resolved(game, deps).await? {
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

    ensure_unlocked(game, deps).await?;
    wait_weth_delay(game, recipient, current_unix_secs(), deps).await?;
    ensure_withdrawn(game, deps).await?;
    close_game(game, deps).await;

    info!(%game, "bond claim pipeline complete");
    Ok(())
}

/// Brings the game to a terminal status, submitting `resolve()` when
/// the game is over and not yet resolved. Returns `true` on terminal
/// status, `false` when the game cannot yet be resolved or when our
/// `resolve()` ran without changing `status`.
async fn ensure_resolved(game: Address, deps: &BondWorkerDeps) -> Result<bool, BondError> {
    if is_resolved(deps.verifier.status(game).await?) {
        return Ok(true);
    }

    if !deps.verifier.game_over(game).await? {
        return Ok(false);
    }

    let request = BondRequest { game_address: game, action: BondAction::Resolve };
    match deps.handle.submit(Submission::Bond(request)).await {
        Ok(_) => Ok(is_resolved(deps.verifier.status(game).await?)),
        Err(source) => Err(BondError::Submit { step: "ensure_resolved", source }),
    }
}

/// Submits `claimCredit()` to unlock the bond. No-op if already unlocked.
async fn ensure_unlocked(game: Address, deps: &BondWorkerDeps) -> Result<(), BondError> {
    if deps.verifier.bond_unlocked(game).await? {
        return Ok(());
    }

    let request = BondRequest { game_address: game, action: BondAction::UnlockCredit };
    deps.handle
        .submit(Submission::Bond(request))
        .await
        .map(|_| ())
        .map_err(|source| BondError::Submit { step: "ensure_unlocked", source })
}

/// Sleeps until `unlock_ts + delay`. No-op if already elapsed.
async fn wait_weth_delay(
    game: Address,
    recipient: Address,
    now_secs: u64,
    deps: &BondWorkerDeps,
) -> Result<(), BondError> {
    let delayed_weth = deps.delayed_weth_resolver.resolve(game).await?;
    // `ensure_unlocked` returns only after `bondUnlocked == true`, and
    // `claimCredit()` atomically calls `DELAYED_WETH.unlock` (which
    // sets `timestamp = block.timestamp`, always non-zero) before
    // flipping the flag, so `unlock_ts > 0` here.
    let (_, unlock_ts) = delayed_weth.withdrawals(game, recipient).await?;
    let delay = delayed_weth.delay().await?;
    let ready_at = unlock_ts.saturating_add(delay.as_secs());
    if now_secs >= ready_at {
        return Ok(());
    }

    debug!(%game, ready_at, now_secs, "waiting DelayedWETH unlock delay");
    tokio::time::sleep(Duration::from_secs(ready_at - now_secs)).await;
    Ok(())
}

/// Submits `claimCredit()` to withdraw the bond. No-op if already claimed.
async fn ensure_withdrawn(game: Address, deps: &BondWorkerDeps) -> Result<(), BondError> {
    if deps.verifier.bond_claimed(game).await? {
        return Ok(());
    }

    let request = BondRequest { game_address: game, action: BondAction::WithdrawCredit };
    deps.handle
        .submit(Submission::Bond(request))
        .await
        .map(|_| ())
        .map_err(|source| BondError::Submit { step: "ensure_withdrawn", source })
}

/// Submits `closeGame()`. Best-effort: failures are logged and ignored.
async fn close_game(game: Address, deps: &BondWorkerDeps) {
    let request = BondRequest { game_address: game, action: BondAction::CloseGame };
    match deps.handle.submit(Submission::Bond(request)).await {
        Ok(tx_hash) => debug!(%game, %tx_hash, "close_game confirmed"),
        Err(e) => warn!(%game, error = %e, "close_game submission failed (best-effort)"),
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
    use alloy_primitives::{B256, U256, address};
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        SubmissionTask,
        test_utils::{
            MockAggregateVerifier, MockDelayedWETH, MockDelayedWETHResolver, MockGameState,
            MockTxManager, receipt_with_status,
        },
    };

    const GAME: Address = address!("00000000000000000000000000000000000000a1");
    const SENDER: Address = address!("00000000000000000000000000000000000000b1");
    const RECIPIENT: Address = address!("00000000000000000000000000000000000000b2");
    const OUTSIDER: Address = address!("00000000000000000000000000000000000000ee");
    const TX_HASH: B256 = B256::repeat_byte(0xAB);
    const WETH_DELAY_SECS: u64 = 7 * 24 * 60 * 60;

    /// Bundles the mocks plus a wired `BondWorkerDeps` so each test
    /// can program one or two fields without re-wiring channels.
    struct Fixture {
        verifier: Arc<MockAggregateVerifier>,
        weth: Arc<MockDelayedWETH>,
        deps: Arc<BondWorkerDeps>,
        cancel: CancellationToken,
        submit_cancel: CancellationToken,
        submit_join: JoinHandle<()>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_tx_manager(MockTxManager::new(SENDER), [RECIPIENT])
        }

        fn with_tx_manager<Tx>(tx_manager: Tx, addrs: impl IntoIterator<Item = Address>) -> Self
        where
            Tx: TxManager + Send + Sync + 'static,
        {
            let verifier = Arc::new(MockAggregateVerifier::new());
            let weth = Arc::new(MockDelayedWETH::new(Duration::from_secs(WETH_DELAY_SECS)));
            let (task, handle) = SubmissionTask::new(tx_manager, 8);
            let submit_cancel = CancellationToken::new();
            let submit_join = tokio::spawn(task.run(submit_cancel.clone()));
            let claim = addrs.into_iter().collect::<HashSet<_>>();
            let delayed_weth_resolver =
                Arc::new(MockDelayedWETHResolver::new(Arc::<MockDelayedWETH>::clone(&weth)));
            let deps = Arc::new(BondWorkerDeps::new(
                Arc::<MockAggregateVerifier>::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
                delayed_weth_resolver as Arc<dyn DelayedWETHResolver>,
                handle,
                claim,
            ));
            Self {
                verifier,
                weth,
                deps,
                cancel: CancellationToken::new(),
                submit_cancel,
                submit_join,
            }
        }

        fn set_state(&self, state: MockGameState) {
            self.verifier.set_game(GAME, state);
        }

        async fn shutdown(self) {
            self.submit_cancel.cancel();
            self.submit_join.await.unwrap();
        }
    }

    /// [`TxManager`] that mutates `verifier`'s game state when its
    /// `send` impl is called, simulating the on-chain effect of a
    /// transaction that landed in a block (Confirmed or Reverted).
    #[derive(std::fmt::Debug, Clone)]
    struct FlippingTxManager {
        verifier: Arc<MockAggregateVerifier>,
        next: Arc<std::sync::Mutex<Option<MockGameState>>>,
        tx_hash: B256,
        success: bool,
    }

    impl FlippingTxManager {
        fn new(
            verifier: Arc<MockAggregateVerifier>,
            next: MockGameState,
            tx_hash: B256,
            success: bool,
        ) -> Self {
            Self { verifier, next: Arc::new(std::sync::Mutex::new(Some(next))), tx_hash, success }
        }
    }

    impl TxManager for FlippingTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            if let Some(next) = self.next.lock().expect("next lock poisoned").take() {
                self.verifier.set_game(GAME, next);
            }
            Ok(receipt_with_status(self.success, self.tx_hash))
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("send_async not exercised by bond worker tests")
        }

        fn sender_address(&self) -> Address {
            SENDER
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

    fn resolved_unlocked() -> MockGameState {
        let mut s = resolved(RECIPIENT);
        s.bond_unlocked = true;
        s
    }

    fn resolved_unlocked_claimed() -> MockGameState {
        let mut s = resolved_unlocked();
        s.bond_claimed = true;
        s
    }

    // ── ensure_resolved ────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_resolved_skips_when_already_resolved() {
        let fx = Fixture::new();
        fx.set_state(resolved(RECIPIENT));

        let result = ensure_resolved(GAME, &fx.deps).await;

        assert!(result.unwrap());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_resolved_returns_false_when_not_game_over() {
        let fx = Fixture::new();
        fx.set_state(in_progress(false));

        let result = ensure_resolved(GAME, &fx.deps).await;

        assert!(!result.unwrap());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_resolved_submits_and_succeeds_when_state_flips() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        verifier.set_game(GAME, in_progress(true));
        let tx_manager =
            FlippingTxManager::new(Arc::clone(&verifier), resolved(RECIPIENT), TX_HASH, true);
        let (task, handle) = SubmissionTask::new(tx_manager, 8);
        let submit_cancel = CancellationToken::new();
        let submit_join = tokio::spawn(task.run(submit_cancel.clone()));
        let weth = Arc::new(MockDelayedWETH::new(Duration::from_secs(WETH_DELAY_SECS)));
        let deps = Arc::new(BondWorkerDeps::new(
            Arc::<MockAggregateVerifier>::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
            Arc::new(MockDelayedWETHResolver::new(weth)) as Arc<dyn DelayedWETHResolver>,
            handle,
            HashSet::from([RECIPIENT]),
        ));

        let result = ensure_resolved(GAME, &deps).await;

        assert!(result.unwrap());

        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn ensure_resolved_returns_false_when_confirmed_but_status_unchanged() {
        // Simulates the contract's `_updateProofCount` early return:
        // resolve() confirms (status=1) but `status` stays IN_PROGRESS.
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(in_progress(true));

        let result = ensure_resolved(GAME, &fx.deps).await;

        assert!(!result.unwrap());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_resolved_propagates_revert() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_revert(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(in_progress(true));

        let result = ensure_resolved(GAME, &fx.deps).await;

        assert!(matches!(
            result,
            Err(BondError::Submit { step: "ensure_resolved", source: SubmitError::Reverted(_) })
        ));
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_resolved_propagates_transport_error() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(in_progress(true));

        let result = ensure_resolved(GAME, &fx.deps).await;

        assert!(matches!(
            result,
            Err(BondError::Submit { step: "ensure_resolved", source: SubmitError::TxManager(_) })
        ));
        fx.shutdown().await;
    }

    // ── ensure_unlocked ─────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_unlocked_skips_when_already_unlocked() {
        let fx = Fixture::new();
        fx.set_state(resolved_unlocked());

        let result = ensure_unlocked(GAME, &fx.deps).await;

        assert!(result.is_ok());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_unlocked_submits_and_succeeds_on_confirmed() {
        // Confirmed implies bondUnlocked=true (contract guarantee), so
        // the worker trusts the outcome without re-reading.
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(resolved(RECIPIENT));

        assert!(ensure_unlocked(GAME, &fx.deps).await.is_ok());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_unlocked_propagates_submit_error() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(resolved(RECIPIENT));

        let result = ensure_unlocked(GAME, &fx.deps).await;

        assert!(matches!(
            result,
            Err(BondError::Submit { step: "ensure_unlocked", source: SubmitError::TxManager(_) })
        ));
        fx.shutdown().await;
    }

    // ── wait_weth_delay ─────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_weth_delay_returns_immediately_when_already_elapsed() {
        let fx = Fixture::new();
        let unlock_ts = 1_000_000;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let now = unlock_ts + WETH_DELAY_SECS + 1;
        let result = wait_weth_delay(GAME, RECIPIENT, now, &fx.deps).await;

        assert!(result.is_ok());
        fx.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn wait_weth_delay_sleeps_for_remaining_window() {
        let fx = Fixture::new();
        let unlock_ts = 1_000_000;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let now = unlock_ts + 100;
        let remaining = WETH_DELAY_SECS - 100;
        let deps = Arc::clone(&fx.deps);
        let task = tokio::spawn(async move { wait_weth_delay(GAME, RECIPIENT, now, &deps).await });

        tokio::time::advance(Duration::from_secs(remaining - 1)).await;
        assert!(!task.is_finished());

        tokio::time::advance(Duration::from_secs(2)).await;
        let result = task.await.expect("task must not panic");
        assert!(result.is_ok());
        fx.shutdown().await;
    }

    // ── ensure_withdrawn ────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_withdrawn_skips_when_already_claimed() {
        let fx = Fixture::new();
        fx.set_state(resolved_unlocked_claimed());

        let result = ensure_withdrawn(GAME, &fx.deps).await;

        assert!(result.is_ok());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_withdrawn_submits_and_succeeds_on_confirmed() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(resolved_unlocked());

        assert!(ensure_withdrawn(GAME, &fx.deps).await.is_ok());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn ensure_withdrawn_propagates_submit_error() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);
        fx.set_state(resolved_unlocked());

        let result = ensure_withdrawn(GAME, &fx.deps).await;

        assert!(matches!(
            result,
            Err(BondError::Submit { step: "ensure_withdrawn", source: SubmitError::TxManager(_) })
        ));
        fx.shutdown().await;
    }

    // ── close_game ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn close_game_submits_and_logs_outcome() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager.clone(), [RECIPIENT]);

        close_game(GAME, &fx.deps).await;

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));
        assert_eq!(calls[0].tx_data, BondAction::CloseGame.to_calldata());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn close_game_swallows_submission_errors() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_revert(TX_HASH);
        let fx = Fixture::with_tx_manager(tx_manager, [RECIPIENT]);

        close_game(GAME, &fx.deps).await;
        fx.shutdown().await;
    }

    // ── run_bond_worker (integration) ───────────────────────────────────

    #[tokio::test]
    async fn pipeline_exits_when_recipient_outside_claim_set() {
        let tx_manager = MockTxManager::new(SENDER);
        let fx = Fixture::with_tx_manager(tx_manager.clone(), [RECIPIENT]);
        fx.set_state(resolved(OUTSIDER));

        run_bond_worker(
            BondCandidate { game_address: GAME, bond_recipient: RECIPIENT },
            Arc::clone(&fx.deps),
            fx.cancel.clone(),
        )
        .await;

        assert!(tx_manager.calls().is_empty());
        fx.shutdown().await;
    }

    #[tokio::test]
    async fn pipeline_exits_when_game_not_resolvable_yet() {
        let tx_manager = MockTxManager::new(SENDER);
        let fx = Fixture::with_tx_manager(tx_manager.clone(), [RECIPIENT]);
        fx.set_state(in_progress(false));

        run_bond_worker(
            BondCandidate { game_address: GAME, bond_recipient: RECIPIENT },
            Arc::clone(&fx.deps),
            fx.cancel.clone(),
        )
        .await;

        assert!(tx_manager.calls().is_empty());
        fx.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_weth_wait_exits_worker() {
        let fx = Fixture::new();
        fx.set_state(resolved_unlocked());
        let unlock_ts = current_unix_secs() + 1;
        fx.weth.set_withdrawal(GAME, RECIPIENT, U256::from(1u64), unlock_ts);

        let cancel = fx.cancel.clone();
        let task = tokio::spawn(run_bond_worker(
            BondCandidate { game_address: GAME, bond_recipient: RECIPIENT },
            Arc::clone(&fx.deps),
            cancel.clone(),
        ));

        tokio::task::yield_now().await;
        cancel.cancel();
        task.await.expect("worker must exit on cancel");
        fx.shutdown().await;
    }
}
