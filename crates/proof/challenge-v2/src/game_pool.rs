//! Game pool: spawns one [`run_game_worker`] per active game address,
//! deduped via [`JoinHandle::is_finished`].

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::Address;
use derive_more::Debug;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{GameInfo, GameWorkerDeps, SubmissionHandle, run_game_worker};

/// Spawns one [`run_game_worker`] per active game address.
#[derive(Debug)]
pub struct GamePool {
    /// Workers per game address.
    #[debug(skip)]
    workers: HashMap<Address, JoinHandle<()>>,
    /// Shared dependencies passed to every worker.
    deps: Arc<GameWorkerDeps>,
    /// Cloned into every spawned worker.
    handle: SubmissionHandle,
}

impl GamePool {
    /// Map size above which `maybe_spawn` sweeps finished entries.
    const GC_THRESHOLD: usize = 256;

    /// Builds a pool wired to `deps` and `handle`.
    pub fn new(deps: Arc<GameWorkerDeps>, handle: SubmissionHandle) -> Self {
        Self { workers: HashMap::new(), deps, handle }
    }

    /// Drains `rx` and spawns one worker per new address.
    /// Exits on `cancel` or closed `rx`.
    pub async fn run(mut self, mut rx: mpsc::Receiver<GameInfo>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                game = rx.recv() => match game {
                    Some(g) => self.maybe_spawn(g),
                    None => return,
                },
            }
        }
    }

    /// Spawns a worker for `game` unless one is already in flight
    /// for the same address.
    fn maybe_spawn(&mut self, game: GameInfo) {
        let address = game.address;

        // A worker is already running for this address.
        if let Some(handle) = self.workers.get(&address)
            && !handle.is_finished()
        {
            debug!(game = %address, "worker already in flight, skipping");
            return;
        }

        // No live worker: spawn one. `insert` overwrites and drops any
        // finished handle still sitting in the slot.
        let worker =
            tokio::spawn(run_game_worker(game, Arc::clone(&self.deps), self.handle.clone()));
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
    use async_trait::async_trait;
    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::{
        GameWorkerConfig, OutputRootError, OutputValidator, ProvingState, SubmissionTask,
        test_utils::{
            MockAggregateVerifier, MockGameState, MockOutputValidator, MockTeeProofProvider,
            MockTxManager, MockZkProofProvider, addr,
        },
    };

    const STARTING_BLOCK: u64 = 100;
    const INTERVAL: u64 = 5;
    const TX_HASH: B256 = B256::repeat_byte(0xAB);

    fn root(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn checkpoint_block(i: u64) -> u64 {
        STARTING_BLOCK + (i + 1) * INTERVAL
    }

    fn config() -> GameWorkerConfig {
        GameWorkerConfig {
            sender_address: addr(0xB2),
            max_proof_retries: 0,
            proof_poll_interval: Duration::from_millis(1),
            max_proof_duration: Duration::from_secs(1),
        }
    }

    fn game_info_zk_only(address: Address, intermediate_roots: Vec<B256>) -> GameInfo {
        let len = intermediate_roots.len() as u64;
        GameInfo {
            address,
            factory_index: 0,
            root_claim: root(99),
            l1_head: root(1),
            l2_block_number: STARTING_BLOCK + INTERVAL * len,
            starting_l2_block: STARTING_BLOCK,
            intermediate_roots: intermediate_roots.into_boxed_slice(),
            intermediate_block_interval: INTERVAL,
            proving_state: ProvingState::ZkOnly,
        }
    }

    /// Validator that responds with a programmed root for any block,
    /// and tracks the call count.
    struct CountingValidator {
        root: B256,
        calls: AtomicU64,
    }

    impl CountingValidator {
        fn new(root: B256) -> Self {
            Self { root, calls: AtomicU64::new(0) }
        }
    }

    #[async_trait]
    impl OutputValidator for CountingValidator {
        async fn compute_output_root(&self, _block: u64) -> Result<B256, OutputRootError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.root)
        }
    }

    /// Validator that signals on every call and parks until the test
    /// releases it. Each call increments [`Self::calls`].
    struct BlockingValidator {
        root: B256,
        started: Arc<Notify>,
        release: Arc<Notify>,
        calls: AtomicU64,
    }

    #[async_trait]
    impl OutputValidator for BlockingValidator {
        async fn compute_output_root(&self, _block: u64) -> Result<B256, OutputRootError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(self.root)
        }
    }

    /// Bundles a verifier + provers; tests build a [`GameWorkerDeps`] from
    /// these plus their chosen validator.
    struct Mocks {
        verifier: Arc<MockAggregateVerifier>,
        zk: Arc<MockZkProofProvider>,
        tee: Arc<MockTeeProofProvider>,
    }

    impl Mocks {
        fn new() -> Self {
            Self {
                verifier: Arc::new(MockAggregateVerifier::new()),
                zk: Arc::new(MockZkProofProvider::new()),
                tee: Arc::new(MockTeeProofProvider::new()),
            }
        }

        fn deps(&self, validator: Arc<dyn OutputValidator>) -> Arc<GameWorkerDeps> {
            Arc::new(GameWorkerDeps::new(
                validator,
                Arc::<MockAggregateVerifier>::clone(&self.verifier),
                Arc::<MockZkProofProvider>::clone(&self.zk),
                Arc::<MockTeeProofProvider>::clone(&self.tee),
                Arc::new(Semaphore::new(8)),
                config(),
            ))
        }
    }

    /// Spawns a [`SubmissionTask`] backed by `tx_manager` and returns
    /// the handle plus the resources tests need to tear it down.
    fn spawn_submission_task(
        tx_manager: MockTxManager,
    ) -> (SubmissionHandle, CancellationToken, JoinHandle<()>) {
        let (task, handle) = SubmissionTask::new(tx_manager, 8);
        let cancel = CancellationToken::new();
        let join = tokio::spawn(task.run(cancel.clone()));
        (handle, cancel, join)
    }

    /// Programs `mocks` so a worker on `address` produces one
    /// [`crate::DisputeRequest`] (mismatch at index 0, ZK proof
    /// succeeds).
    fn arrange_violation(mocks: &Mocks, validator: &MockOutputValidator, address: Address) {
        mocks.verifier.set_game(address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
        validator.set(checkpoint_block(0), root(99));
        validator.set(STARTING_BLOCK, root(50));
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_succeeded(vec![0xAA]);
    }

    /// Programs the verifier so the worker classifies as `ZkOnly`
    /// and exits via the no-violation branch (every claimed root
    /// matches what the validator returns).
    fn arrange_no_violation(mocks: &Mocks, address: Address) {
        mocks.verifier.set_game(address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
    }

    #[tokio::test]
    async fn single_game_spawns_one_worker_that_submits_one_tx() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let address = addr(0xA1);
        arrange_violation(&mocks, &validator, address);

        let tx_manager = MockTxManager::new(addr(0xB2));
        tx_manager.push_success(TX_HASH);
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager.clone());

        let (game_tx, game_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = GamePool::new(mocks.deps(validator), handle);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();

        // Wait for the worker to submit and the task to record the call.
        for _ in 0..64 {
            if !tx_manager.calls().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(address));

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn distinct_addresses_each_get_their_own_worker() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let address_a = addr(0xA1);
        let address_b = addr(0xA2);
        arrange_violation(&mocks, &validator, address_a);
        arrange_violation(&mocks, &validator, address_b);

        let tx_manager = MockTxManager::new(addr(0xB2));
        tx_manager.push_success(TX_HASH);
        tx_manager.push_success(TX_HASH);
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager.clone());

        let (game_tx, game_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = GamePool::new(mocks.deps(validator), handle);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        game_tx.send(game_info_zk_only(address_a, vec![root(10)])).await.unwrap();
        game_tx.send(game_info_zk_only(address_b, vec![root(10)])).await.unwrap();

        for _ in 0..128 {
            if tx_manager.calls().len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut targets = tx_manager.calls().into_iter().filter_map(|c| c.to).collect::<Vec<_>>();
        targets.sort();
        assert_eq!(targets, vec![address_a, address_b]);

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn dedups_same_address_while_a_worker_is_still_in_flight() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let validator = Arc::new(BlockingValidator {
            root: root(10),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: AtomicU64::new(0),
        });
        let mocks = Mocks::new();
        let address = addr(0xA1);
        mocks.verifier.set_game(address, MockGameState::in_progress(Address::ZERO, addr(2), 0));

        let tx_manager = MockTxManager::new(addr(0xB2));
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager);

        let (game_tx, game_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let validator_handle = Arc::clone(&validator);
        let pool =
            GamePool::new(mocks.deps(Arc::clone(&validator) as Arc<dyn OutputValidator>), handle);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        // First send: worker spawns, blocks inside the validator.
        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();
        started.notified().await;

        // Second send same address while the first worker is parked:
        // the pool must dedup.
        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();
        // Yield enough times that a second worker, if spawned, would
        // have entered the validator and bumped the counter.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(validator_handle.calls.load(Ordering::SeqCst), 1);

        // Release + tear down.
        release.notify_waiters();
        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn re_spawns_same_address_after_previous_worker_finished() {
        let mocks = Mocks::new();
        let validator = Arc::new(CountingValidator::new(root(10)));
        let address = addr(0xA1);
        arrange_no_violation(&mocks, address);

        let tx_manager = MockTxManager::new(addr(0xB2));
        let (handle, submit_cancel, submit_join) = spawn_submission_task(tx_manager);

        let (game_tx, game_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let validator_handle = Arc::clone(&validator);
        let pool =
            GamePool::new(mocks.deps(Arc::clone(&validator) as Arc<dyn OutputValidator>), handle);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        // First worker: completes after one call to the validator.
        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();
        for _ in 0..32 {
            if validator_handle.calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Extra yields so the spawned worker fully terminates and the
        // join handle flips to finished before the next message.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // Second send same address: the previous handle is finished,
        // so a fresh worker spawns.
        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();
        for _ in 0..32 {
            if validator_handle.calls.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(validator_handle.calls.load(Ordering::SeqCst), 2);

        cancel.cancel();
        pool_handle.await.unwrap();
        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let (handle, submit_cancel, submit_join) =
            spawn_submission_task(MockTxManager::new(addr(0xB2)));
        let pool = GamePool::new(mocks.deps(validator), handle);
        let (_game_tx, game_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));
        cancel.cancel();
        pool_handle.await.expect("run must exit cleanly on cancel");

        submit_cancel.cancel();
        submit_join.await.unwrap();
    }

    #[tokio::test]
    async fn closed_input_channel_exits_cleanly() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let (handle, submit_cancel, submit_join) =
            spawn_submission_task(MockTxManager::new(addr(0xB2)));
        let pool = GamePool::new(mocks.deps(validator), handle);
        let (game_tx, game_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let pool_handle = tokio::spawn(pool.run(game_rx, cancel));
        drop(game_tx);
        pool_handle.await.expect("run must exit when senders drop");

        submit_cancel.cancel();
        submit_join.await.unwrap();
    }
}
