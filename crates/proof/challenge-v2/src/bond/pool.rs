//! Bond pool: spawns one [`run_bond_worker`] per game address with a
//! bond to claim, deduped via [`JoinHandle::is_finished`].

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::Address;
use derive_more::Debug;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{BondCandidate, BondWorkerDeps, run_bond_worker};

/// Spawns one [`run_bond_worker`] per game address with a bond to claim.
#[derive(Debug)]
pub struct BondPool {
    /// Workers per game address.
    #[debug(skip)]
    workers: HashMap<Address, JoinHandle<()>>,
    /// Shared dependencies passed to every worker.
    deps: Arc<BondWorkerDeps>,
}

impl BondPool {
    /// Map size above which `maybe_spawn` sweeps finished entries.
    const GC_THRESHOLD: usize = 256;

    /// Builds a pool wired to `deps`.
    pub fn new(deps: Arc<BondWorkerDeps>) -> Self {
        Self { workers: HashMap::new(), deps }
    }

    /// Drains `rx` and spawns one worker per new address.
    /// Exits on `cancel` or closed `rx`.
    pub async fn run(mut self, mut rx: mpsc::Receiver<BondCandidate>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                candidate = rx.recv() => match candidate {
                    Some(c) => self.maybe_spawn(c, &cancel),
                    None => return,
                },
            }
        }
    }

    /// Spawns a worker for `candidate` unless one is already in flight
    /// for the same game address.
    fn maybe_spawn(&mut self, candidate: BondCandidate, cancel: &CancellationToken) {
        let address = candidate.game_address;

        // A worker is already running for this address.
        if let Some(handle) = self.workers.get(&address)
            && !handle.is_finished()
        {
            debug!(game = %address, "bond worker already in flight, skipping");
            return;
        }

        // No live worker: spawn one. `insert` overwrites and drops any
        // finished handle still sitting in the slot.
        let worker =
            tokio::spawn(run_bond_worker(candidate, Arc::clone(&self.deps), cancel.clone()));
        self.workers.insert(address, worker);

        // GC finished workers to keep the map bounded.
        if self.workers.len() > Self::GC_THRESHOLD {
            let before = self.workers.len();
            self.workers.retain(|_, h| !h.is_finished());
            info!(swept = before - self.workers.len(), kept = self.workers.len(), "gc sweep");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use alloy_primitives::B256;
    use base_proof_contracts::AggregateVerifierClient;
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
    use tokio::sync::Notify;

    use super::*;
    use crate::{
        DelayedWETHResolver, SubmissionHandle, SubmissionTask,
        test_utils::{
            MockAggregateVerifier, MockDelayedWETH, MockDelayedWETHResolver, MockGameState,
            MockTxManager, addr, receipt_with_status,
        },
    };

    const SENDER: Address = Address::repeat_byte(0xB2);
    const RECIPIENT: Address = Address::repeat_byte(0xBB);
    const TX_HASH: B256 = B256::repeat_byte(0xAB);
    const WETH_DELAY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Game state primed so `ensure_resolved` submits one `resolve()`
    /// then exits with `Ok(false)` once `status` re-reads as `IN_PROGRESS`
    /// (the contract's `_updateProofCount` early-return path).
    fn resolvable_in_progress() -> MockGameState {
        let mut s = MockGameState::in_progress(Address::ZERO, Address::ZERO, 0);
        s.game_over = true;
        s.bond_recipient = RECIPIENT;
        s
    }

    fn candidate(game: Address) -> BondCandidate {
        BondCandidate { game_address: game, bond_recipient: RECIPIENT }
    }

    fn spawn_submission_task<Tx>(
        tx_manager: Tx,
    ) -> (SubmissionHandle, CancellationToken, JoinHandle<()>)
    where
        Tx: TxManager + Send + Sync + 'static,
    {
        let (task, handle) = SubmissionTask::new(tx_manager, 8);
        let cancel = CancellationToken::new();
        let join = tokio::spawn(task.run(cancel.clone()));
        (handle, cancel, join)
    }

    fn deps(verifier: Arc<MockAggregateVerifier>, handle: SubmissionHandle) -> Arc<BondWorkerDeps> {
        let weth = Arc::new(MockDelayedWETH::new(WETH_DELAY));
        Arc::new(BondWorkerDeps::new(
            verifier as Arc<dyn AggregateVerifierClient>,
            Arc::new(MockDelayedWETHResolver::new(weth)) as Arc<dyn DelayedWETHResolver>,
            handle,
            std::iter::once(RECIPIENT).collect(),
        ))
    }

    /// `TxManager` whose `send` parks until released, used to keep a
    /// worker in flight for the dedup test.
    #[derive(Clone, std::fmt::Debug)]
    struct BlockingTxManager {
        started: Arc<Notify>,
        release: Arc<Notify>,
        calls: Arc<AtomicU64>,
    }

    impl TxManager for BlockingTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(receipt_with_status(true, TX_HASH))
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("send_async not exercised by bond pool tests")
        }

        fn sender_address(&self) -> Address {
            SENDER
        }
    }

    #[tokio::test]
    async fn single_candidate_spawns_one_worker_that_submits_one_tx() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let game = addr(0xA1);
        verifier.set_game(game, resolvable_in_progress());

        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager.clone());

        let (tx, rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = BondPool::new(deps(verifier, handle));
        let pool_handle = tokio::spawn(pool.run(rx, cancel.clone()));

        tx.send(candidate(game)).await.unwrap();

        for _ in 0..64 {
            if !tx_manager.calls().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(game));

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn distinct_addresses_each_get_their_own_worker() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let game_a = addr(0xA1);
        let game_b = addr(0xA2);
        verifier.set_game(game_a, resolvable_in_progress());
        verifier.set_game(game_b, resolvable_in_progress());

        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        tx_manager.push_success(TX_HASH);
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager.clone());

        let (tx, rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = BondPool::new(deps(verifier, handle));
        let pool_handle = tokio::spawn(pool.run(rx, cancel.clone()));

        tx.send(candidate(game_a)).await.unwrap();
        tx.send(candidate(game_b)).await.unwrap();

        for _ in 0..128 {
            if tx_manager.calls().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut targets = tx_manager.calls().into_iter().filter_map(|c| c.to).collect::<Vec<_>>();
        targets.sort();
        assert_eq!(targets, vec![game_a, game_b]);

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn dedups_same_address_while_a_worker_is_still_in_flight() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let game = addr(0xA1);
        verifier.set_game(game, resolvable_in_progress());

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicU64::new(0));
        let tx_manager = BlockingTxManager {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: Arc::clone(&calls),
        };
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager);

        let (tx, rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = BondPool::new(deps(verifier, handle));
        let pool_handle = tokio::spawn(pool.run(rx, cancel.clone()));

        // First send: worker spawns, parks inside the tx manager.
        tx.send(candidate(game)).await.unwrap();
        started.notified().await;

        // Second send same address while the first worker is parked:
        // the pool must dedup.
        tx.send(candidate(game)).await.unwrap();
        // Yield enough times that a second worker, if spawned, would
        // have reached the tx manager and bumped the counter.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        release.notify_waiters();
        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn re_spawns_same_address_after_previous_worker_finished() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let game = addr(0xA1);
        verifier.set_game(game, resolvable_in_progress());

        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        tx_manager.push_success(TX_HASH);
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager.clone());

        let (tx, rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = BondPool::new(deps(verifier, handle));
        let pool_handle = tokio::spawn(pool.run(rx, cancel.clone()));

        // First worker: submits one resolve() and exits.
        tx.send(candidate(game)).await.unwrap();
        for _ in 0..64 {
            if !tx_manager.calls().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Extra yields so the spawned worker fully terminates and the
        // join handle flips to finished before the next message.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // Second send same address: previous handle is finished, fresh
        // worker spawns.
        tx.send(candidate(game)).await.unwrap();
        for _ in 0..64 {
            if tx_manager.calls().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(tx_manager.calls().len(), 2);

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let (handle, submit_cancel, submit_join) =
            spawn_submission_task(MockTxManager::new(SENDER));
        let pool = BondPool::new(deps(verifier, handle));
        let (_tx, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let pool_handle = tokio::spawn(pool.run(rx, cancel.clone()));
        cancel.cancel();
        pool_handle.await.expect("run must exit cleanly on cancel");

        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn closed_input_channel_exits_cleanly() {
        let verifier = Arc::new(MockAggregateVerifier::new());
        let (handle, submit_cancel, submit_join) =
            spawn_submission_task(MockTxManager::new(SENDER));
        let pool = BondPool::new(deps(verifier, handle));
        let (tx, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let pool_handle = tokio::spawn(pool.run(rx, cancel));
        drop(tx);
        pool_handle.await.expect("run must exit when senders drop");

        submit_cancel.cancel();
        submit_join.await.unwrap();
    }
}
