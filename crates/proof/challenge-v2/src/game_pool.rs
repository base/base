//! Game pool: spawns one [`run_game_worker`] per active game address,
//! deduped via [`JoinHandle::is_finished`].

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::Address;
use derive_more::Debug;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{DisputeRequest, GameInfo, WorkerDeps, run_game_worker};

/// Spawns one [`run_game_worker`] per active game address.
#[derive(Debug)]
pub struct GamePool {
    /// Workers per game address.
    #[debug(skip)]
    workers: HashMap<Address, JoinHandle<()>>,
    /// Shared dependencies passed to every worker.
    deps: Arc<WorkerDeps>,
    /// Outbound channel to the [`crate::SubmissionTask`].
    #[debug(skip)]
    submit_tx: mpsc::Sender<DisputeRequest>,
}

impl GamePool {
    /// Map size above which `maybe_spawn` sweeps finished entries.
    const GC_THRESHOLD: usize = 256;

    /// Builds a pool wired to `deps` and `submit_tx`.
    pub fn new(deps: Arc<WorkerDeps>, submit_tx: mpsc::Sender<DisputeRequest>) -> Self {
        Self { workers: HashMap::new(), deps, submit_tx }
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

        // Live worker already covers this address: skip the duplicate
        // emit (scanner re-publishes every active game per tick).
        if let Some(handle) = self.workers.get(&address)
            && !handle.is_finished()
        {
            debug!(game = %address, "worker already in flight, skipping");
            return;
        }

        // No live worker: spawn one. `insert` overwrites and drops any
        // finished handle still sitting in the slot.
        let handle =
            tokio::spawn(run_game_worker(game, Arc::clone(&self.deps), self.submit_tx.clone()));
        self.workers.insert(address, handle);

        // GC finished workers to keep the map bounded.
        if self.workers.len() > Self::GC_THRESHOLD {
            self.workers.retain(|_, h| !h.is_finished());
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
    use tokio::sync::Notify;

    use super::*;
    use crate::{
        GameSituation, OutputValidator, ValidatorError, WorkerConfig,
        test_utils::{
            MockAggregateVerifier, MockGameState, MockOutputValidator, MockTeeProofProvider,
            MockZkProofProvider, addr,
        },
    };

    const STARTING_BLOCK: u64 = 100;
    const INTERVAL: u64 = 5;

    fn root(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn checkpoint_block(i: u64) -> u64 {
        STARTING_BLOCK + (i + 1) * INTERVAL
    }

    fn config() -> WorkerConfig {
        WorkerConfig {
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
            situation: GameSituation::ZkOnly,
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
        async fn compute_output_root(&self, _block: u64) -> Result<B256, ValidatorError> {
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
        async fn compute_output_root(&self, _block: u64) -> Result<B256, ValidatorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(self.root)
        }
    }

    /// Bundles a verifier + provers; tests build a [`WorkerDeps`] from
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

        fn deps(&self, validator: Arc<dyn OutputValidator>) -> Arc<WorkerDeps> {
            Arc::new(WorkerDeps::new(
                validator,
                Arc::<MockAggregateVerifier>::clone(&self.verifier),
                Arc::<MockZkProofProvider>::clone(&self.zk),
                Arc::<MockTeeProofProvider>::clone(&self.tee),
                config(),
            ))
        }
    }

    /// Programs `mocks` so a worker on `game` produces one
    /// [`DisputeRequest`] (mismatch at index 0, ZK proof succeeds).
    fn arrange_violation(mocks: &Mocks, validator: &MockOutputValidator, address: Address) {
        mocks.verifier.set_game(address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
        validator.set(checkpoint_block(0), root(99));
        validator.set(STARTING_BLOCK, root(50));
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_succeeded(vec![0xAA]);
    }

    /// Programs `mocks` so the worker exits via the `no_violation`
    /// branch (every claimed root matches).
    fn arrange_no_violation(mocks: &Mocks, address: Address, situation: GameSituation) {
        let state = match situation {
            GameSituation::ZkOnly => MockGameState::in_progress(Address::ZERO, addr(2), 0),
            GameSituation::TeeOnly => MockGameState::in_progress(addr(1), Address::ZERO, 0),
            other => panic!("arrange_no_violation: unsupported situation {other:?}"),
        };
        mocks.verifier.set_game(address, state);
    }

    #[tokio::test]
    async fn single_game_spawns_one_worker_that_dispatches_one_request() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let address = addr(0xA1);
        arrange_violation(&mocks, &validator, address);

        let (game_tx, game_rx) = mpsc::channel(4);
        let (submit_tx, mut submit_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = GamePool::new(mocks.deps(validator), submit_tx);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();

        let req = tokio::time::timeout(Duration::from_secs(1), submit_rx.recv())
            .await
            .expect("worker must dispatch within timeout")
            .expect("submit channel must stay open");
        assert_eq!(req.game_address, address);

        cancel.cancel();
        pool_handle.await.unwrap();
    }

    #[tokio::test]
    async fn distinct_addresses_each_get_their_own_worker() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let address_a = addr(0xA1);
        let address_b = addr(0xA2);
        arrange_violation(&mocks, &validator, address_a);
        arrange_violation(&mocks, &validator, address_b);

        let (game_tx, game_rx) = mpsc::channel(4);
        let (submit_tx, mut submit_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let pool = GamePool::new(mocks.deps(validator), submit_tx);
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        game_tx.send(game_info_zk_only(address_a, vec![root(10)])).await.unwrap();
        game_tx.send(game_info_zk_only(address_b, vec![root(10)])).await.unwrap();

        let mut seen = Vec::new();
        for _ in 0..2 {
            let req = tokio::time::timeout(Duration::from_secs(1), submit_rx.recv())
                .await
                .expect("two requests must dispatch")
                .expect("submit channel must stay open");
            seen.push(req.game_address);
        }
        seen.sort();
        assert_eq!(seen, vec![address_a, address_b]);

        cancel.cancel();
        pool_handle.await.unwrap();
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
        // Verifier resolves; validator returns r0 == on-chain root, so
        // the worker exits via the `no_violation` branch once released.
        arrange_no_violation(&mocks, address, GameSituation::ZkOnly);

        let (game_tx, game_rx) = mpsc::channel(4);
        let (submit_tx, _submit_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let validator_handle = Arc::clone(&validator);
        let pool = GamePool::new(
            mocks.deps(Arc::clone(&validator) as Arc<dyn OutputValidator>),
            submit_tx,
        );
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
    }

    #[tokio::test]
    async fn re_spawns_same_address_after_previous_worker_finished() {
        let mocks = Mocks::new();
        let validator = Arc::new(CountingValidator::new(root(10)));
        let address = addr(0xA1);
        arrange_no_violation(&mocks, address, GameSituation::ZkOnly);

        let (game_tx, game_rx) = mpsc::channel(4);
        let (submit_tx, _submit_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let validator_handle = Arc::clone(&validator);
        let pool = GamePool::new(
            mocks.deps(Arc::clone(&validator) as Arc<dyn OutputValidator>),
            submit_tx,
        );
        let pool_handle = tokio::spawn(pool.run(game_rx, cancel.clone()));

        // First worker: completes after one call to the validator.
        game_tx.send(game_info_zk_only(address, vec![root(10)])).await.unwrap();
        // Wait for the worker to complete (`is_finished == true`).
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
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let pool = GamePool::new(mocks.deps(validator), mpsc::channel(1).0);
        let (_game_tx, game_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(pool.run(game_rx, cancel.clone()));
        cancel.cancel();
        handle.await.expect("run must exit cleanly on cancel");
    }

    #[tokio::test]
    async fn closed_input_channel_exits_cleanly() {
        let mocks = Mocks::new();
        let validator = Arc::new(MockOutputValidator::new());
        let pool = GamePool::new(mocks.deps(validator), mpsc::channel(1).0);
        let (game_tx, game_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(pool.run(game_rx, cancel));
        drop(game_tx);
        handle.await.expect("run must exit when senders drop");
    }
}
