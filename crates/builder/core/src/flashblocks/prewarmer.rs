//! Speculative transaction execution for warming the shared execution cache.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
};

use alloy_primitives::B256;
use base_common_consensus::BaseTransactionSigned;
use base_common_evm::BaseSpecId;
use base_execution_evm::BaseEvmConfig;
use reth_evm::{ConfigureEvm, Evm, EvmEnv};
use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{BestTransactions, PoolTransaction};
use tracing::trace;

const WORK_AHEAD_PER_WORKER: usize = 2;

/// Handle for a bounded, cancellable transaction-prewarming job.
///
/// The job owns and drains an independent [`BestTransactions`] iterator on a blocking coordinator
/// thread. Candidate transactions execute concurrently on [`TaskExecutor::prewarming_pool`] with at
/// most twice the pool's worker count retained in flight. The callback normally invokes
/// [`PrewarmingExecutionContext::prewarm_transactions`].
///
/// Starting a job does not wait for speculative execution. Call [`Self::stop_and_wait`] before
/// publishing a payload to ensure all speculative workers and their execution-cache handles have
/// been released. Dropping the handle is a safe fallback that also cancels and waits for cleanup.
#[derive(Debug)]
pub struct TransactionPrewarmer {
    stop: Arc<AtomicBool>,
    completion: Option<Receiver<()>>,
}

impl TransactionPrewarmer {
    /// Starts draining `transactions` and prewarming candidates with `prewarm`.
    ///
    /// The iterator must be independent from the canonical build iterator. `prewarm` may run
    /// concurrently; the job discards its return value because prewarming is only an optimization.
    pub fn start<Txs, F, R>(executor: &TaskExecutor, transactions: Txs, prewarm: F) -> Self
    where
        Txs: BestTransactions + 'static,
        Txs::Item: Send,
        F: Fn(&Txs::Item) -> R + Send + Sync + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let coordinator_stop = Arc::clone(&stop);
        let (completion_tx, completion) = mpsc::channel();
        let coordinator_executor = executor.clone();

        executor.spawn_blocking(move || {
            let run = move || {
                let pool = coordinator_executor.prewarming_pool();
                let max_in_flight =
                    pool.current_num_threads().saturating_mul(WORK_AHEAD_PER_WORKER).max(1);
                let mut transactions = transactions;

                pool.in_place_scope(|scope| {
                    let (worker_tx, worker_rx) = mpsc::channel();
                    let mut exhausted = false;
                    let mut in_flight = 0;

                    loop {
                        while !exhausted
                            && in_flight < max_in_flight
                            && !coordinator_stop.load(Ordering::Acquire)
                        {
                            let Some(transaction) = transactions.next() else {
                                exhausted = true;
                                break;
                            };
                            let worker_tx = worker_tx.clone();
                            let prewarm = &prewarm;
                            scope.spawn(move |_| {
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    drop(prewarm(&transaction));
                                }));
                                let _ = worker_tx.send(());
                                if result.is_err() {
                                    trace!(
                                        target: "payload_builder",
                                        "transaction prewarming worker panicked",
                                    );
                                }
                            });
                            in_flight += 1;
                        }

                        if in_flight == 0 {
                            break;
                        }
                        if worker_rx.recv().is_err() {
                            break;
                        }
                        in_flight -= 1;
                    }
                });
            };

            let _ = catch_unwind(AssertUnwindSafe(run));
            let _ = completion_tx.send(());
        });

        Self { stop, completion: Some(completion) }
    }

    /// Cooperatively stops fetching and scheduling new candidates without waiting for workers.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Waits for iterator exhaustion and releases all job-owned resources.
    ///
    /// An iterator receiving continuous pool updates may not exhaust; use [`Self::stop_and_wait`]
    /// when bounded cleanup is required.
    pub fn wait(mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.recv();
        }
    }

    /// Stops scheduling new candidates and waits for already scheduled work to finish.
    pub fn stop_and_wait(self) {
        self.stop();
        self.wait();
    }
}

impl Drop for TransactionPrewarmer {
    fn drop(&mut self) {
        self.stop();
        if let Some(completion) = self.completion.take() {
            let _ = completion.recv();
        }
    }
}

/// Context for speculatively executing transactions against parent state.
///
/// Opening a state provider is the dominant cost of prewarming, so a single provider and EVM are
/// built once and reused for every transaction in a batch, mirroring how reth's own prewarming
/// (`reth_engine_tree`) amortizes EVM construction across a worker's transactions. Each
/// transaction still executes independently through [`Evm::transact`], which never commits: state
/// changes and execution results are discarded, and transactions are never chained together, so
/// only reads populated through the shared [`ExecutionCache`] are retained. Deliberately not
/// chaining keeps every transaction warming reads against the same real parent state that the
/// actual sequential execution pass will observe, rather than a speculative intermediate state
/// that pass may never produce.
#[derive(Clone, Debug)]
pub struct PrewarmingExecutionContext<Client> {
    client: Client,
    parent_hash: B256,
    evm_config: BaseEvmConfig,
    evm_env: EvmEnv<BaseSpecId>,
    execution_cache: ExecutionCache,
}

impl<Client> PrewarmingExecutionContext<Client>
where
    Client: StateProviderFactory,
{
    /// Creates a speculative execution context for one payload build.
    pub const fn new(
        client: Client,
        parent_hash: B256,
        evm_config: BaseEvmConfig,
        evm_env: EvmEnv<BaseSpecId>,
        execution_cache: ExecutionCache,
    ) -> Self {
        Self { client, parent_hash, evm_config, evm_env, execution_cache }
    }

    /// Executes each of `transactions` against parent state to populate the shared execution
    /// cache.
    ///
    /// The state provider and EVM are opened once and reused for every transaction in the batch
    /// rather than per transaction, since opening a state provider dominates the per-transaction
    /// cost. Nonce and balance checks are disabled so transactions that depend on earlier block
    /// state can still warm useful reads. Every transaction executes independently against the
    /// same parent state; execution results and state changes are always discarded, and
    /// transactions are never chained.
    ///
    /// Returns the number of transactions that executed successfully. Failing to open the parent
    /// state, or a rejected transaction, is deliberately non-fatal because prewarming is only an
    /// optimization.
    pub fn prewarm_transactions<'a, T>(
        &self,
        transactions: impl IntoIterator<Item = &'a T>,
    ) -> usize
    where
        T: PoolTransaction<Consensus = BaseTransactionSigned> + 'a,
    {
        let mut transactions = transactions.into_iter().peekable();
        if transactions.peek().is_none() {
            return 0;
        }

        let state_provider = match self.client.state_by_block_hash(self.parent_hash) {
            Ok(provider) => provider,
            Err(error) => {
                trace!(
                    target: "payload_builder",
                    error = %error,
                    parent_hash = ?self.parent_hash,
                    "failed to open parent state for transaction prewarming",
                );
                return 0;
            }
        };
        let state_provider =
            CachedStateProvider::new_prewarm(state_provider, self.execution_cache.clone());
        let database = StateProviderDatabase::new(state_provider);

        let mut evm_env = self.evm_env.clone();
        evm_env.cfg_env.disable_nonce_check = true;
        evm_env.cfg_env.disable_balance_check = true;
        let mut evm = self.evm_config.evm_with_env(database, evm_env);

        let mut warmed = 0;
        for transaction in transactions {
            let transaction = transaction.consensus_ref();
            match evm.transact(transaction) {
                Ok(_) => warmed += 1,
                Err(error) => {
                    trace!(
                        target: "payload_builder",
                        error = %error,
                        transaction_hash = ?transaction.tx_hash(),
                        "failed to speculatively execute transaction for prewarming",
                    );
                }
            }
        }
        warmed
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use alloy_consensus::{Header, SignableTransaction, TxEip1559};
    use alloy_eips::{Encodable2718, eip1559::MIN_PROTOCOL_BASE_FEE};
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex, keccak256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction};
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
    use base_execution_txpool::BasePooledTransaction;
    use reth_chainspec::ChainSpec;
    use reth_evm::ConfigureEvm;
    use reth_execution_cache::{CachedStatus, ExecutionCache, SavedCache};
    use reth_primitives_traits::Recovered;
    use reth_provider::{
        AccountReader,
        test_utils::{ExtendedAccount, MockEthProvider},
    };
    use reth_tasks::TaskExecutor;
    use reth_transaction_pool::{BestTransactions, error::InvalidPoolTransactionError};

    use super::{PrewarmingExecutionContext, TransactionPrewarmer, WORK_AHEAD_PER_WORKER};

    const CHAIN_ID: u64 = 901;
    const CONTRACT_STORAGE_VALUE: u64 = 42;
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    type TestClient = MockEthProvider<base_common_consensus::BasePrimitives>;

    #[derive(Debug)]
    struct TestBestTransactions {
        transactions: std::vec::IntoIter<usize>,
        fetched: Arc<AtomicUsize>,
    }

    impl TestBestTransactions {
        fn new(count: usize) -> (Self, Arc<AtomicUsize>) {
            let fetched = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    transactions: (0..count).collect::<Vec<_>>().into_iter(),
                    fetched: Arc::clone(&fetched),
                },
                fetched,
            )
        }
    }

    impl Iterator for TestBestTransactions {
        type Item = usize;

        fn next(&mut self) -> Option<Self::Item> {
            let transaction = self.transactions.next();
            if transaction.is_some() {
                self.fetched.fetch_add(1, Ordering::Relaxed);
            }
            transaction
        }
    }

    impl BestTransactions for TestBestTransactions {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: InvalidPoolTransactionError) {}

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    #[derive(Debug, Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().expect("lock gate");
            while !*open {
                open = self.changed.wait(open).expect("wait for gate");
            }
        }

        fn open(&self) {
            *self.open.lock().expect("lock gate") = true;
            self.changed.notify_all();
        }
    }

    struct Fixture {
        context: PrewarmingExecutionContext<TestClient>,
        client: TestClient,
        cache: SavedCache,
        transaction: BasePooledTransaction,
        sender: Address,
        contract: Address,
        code_hash: B256,
    }

    impl Fixture {
        fn new(transaction_nonce: u64) -> Self {
            Self::with_transaction_chain_id(transaction_nonce, CHAIN_ID)
        }

        fn with_transaction_chain_id(transaction_nonce: u64, transaction_chain_id: u64) -> Self {
            let genesis: serde_json::Value = serde_json::json!({
                "config": { "chainId": CHAIN_ID },
                "gasLimit": "0x1C9C380",
                "timestamp": "0x0"
            });
            let genesis = serde_json::from_value(genesis).expect("valid genesis");
            let chain_spec = Arc::new(BaseChainSpec::from(
                ChainSpec::builder()
                    .chain(CHAIN_ID.into())
                    .genesis(genesis)
                    .cancun_activated()
                    .build(),
            ));
            let evm_config = BaseEvmConfig::base(chain_spec);
            let parent = Header {
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(MIN_PROTOCOL_BASE_FEE),
                ..Default::default()
            };
            let evm_env = evm_config
                .next_evm_env(
                    &parent,
                    &BaseNextBlockEnvAttributes {
                        timestamp: 1,
                        suggested_fee_recipient: Address::random(),
                        prev_randao: B256::random(),
                        gas_limit: parent.gas_limit,
                        parent_beacon_block_root: None,
                        extra_data: Default::default(),
                    },
                )
                .expect("valid next block environment");

            let signer = PrivateKeySigner::random();
            let sender = signer.address();
            let contract = Address::random();
            let bytecode: Bytes = hex!("60005400").into();
            let code_hash = keccak256(&bytecode);
            let client = TestClient::new();
            client.add_account(
                contract,
                ExtendedAccount::new(0, U256::ZERO)
                    .with_bytecode(bytecode)
                    .extend_storage([(B256::ZERO, U256::from(CONTRACT_STORAGE_VALUE))]),
            );

            let transaction = TxEip1559 {
                chain_id: transaction_chain_id,
                nonce: transaction_nonce,
                gas_limit: 100_000,
                max_fee_per_gas: MIN_PROTOCOL_BASE_FEE as u128,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(contract),
                ..Default::default()
            };
            let signature =
                signer.sign_hash_sync(&transaction.signature_hash()).expect("sign transaction");
            let signed = BaseTransactionSigned::new_unhashed(
                BaseTypedTransaction::Eip1559(transaction),
                signature,
            );
            let recovered = Recovered::new_unchecked(signed, sender);
            let encoded_length = recovered.encode_2718_len();
            let transaction = BasePooledTransaction::new(recovered, encoded_length);

            let cache = SavedCache::new(B256::ZERO, ExecutionCache::new(1_000_000));
            let context = PrewarmingExecutionContext::new(
                client.clone(),
                B256::ZERO,
                evm_config,
                evm_env,
                cache.cache().clone(),
            );

            Self { context, client, cache, transaction, sender, contract, code_hash }
        }
    }

    #[test]
    fn job_starts_without_waiting_and_executes_concurrently() {
        let executor = TaskExecutor::test();
        let (transactions, _) = TestBestTransactions::new(4);
        let gate = Arc::new(Gate::default());
        let callback_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::channel();

        let job = TransactionPrewarmer::start(&executor, transactions, move |_| {
            started_tx.send(()).expect("report started worker");
            callback_gate.wait();
        });

        let first_started = started_rx.recv_timeout(TEST_TIMEOUT);
        let second_started = started_rx.recv_timeout(TEST_TIMEOUT);
        gate.open();
        job.wait();
        first_started.expect("first worker should start");
        second_started.expect("second worker should start concurrently");
    }

    #[test]
    fn job_bounds_lookahead_and_cancellation_stops_fetching() {
        let executor = TaskExecutor::test();
        let worker_count = executor.prewarming_pool().current_num_threads();
        let (transactions, fetched) = TestBestTransactions::new(20);
        let gate = Arc::new(Gate::default());
        let callback_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::channel();

        let job = TransactionPrewarmer::start(&executor, transactions, move |_| {
            started_tx.send(()).expect("report started worker");
            callback_gate.wait();
        });

        let workers_started =
            (0..worker_count).all(|_| started_rx.recv_timeout(TEST_TIMEOUT).is_ok());
        let lookahead = worker_count * WORK_AHEAD_PER_WORKER;
        let deadline = Instant::now() + TEST_TIMEOUT;
        while fetched.load(Ordering::Relaxed) < lookahead && Instant::now() < deadline {
            thread::yield_now();
        }
        let fetched_before_stop = fetched.load(Ordering::Relaxed);

        job.stop();
        gate.open();
        job.wait();
        assert!(workers_started, "every prewarming worker should start");
        assert_eq!(fetched_before_stop, lookahead, "coordinator should fill its bounded lookahead");
        assert_eq!(
            fetched.load(Ordering::Relaxed),
            fetched_before_stop,
            "cancellation must not fetch another candidate as workers finish"
        );
    }

    #[test]
    fn job_exhaustion_processes_every_candidate() {
        let executor = TaskExecutor::test();
        let (transactions, _) = TestBestTransactions::new(8);
        let processed = Arc::new(Mutex::new(Vec::new()));
        let callback_processed = Arc::clone(&processed);

        TransactionPrewarmer::start(&executor, transactions, move |transaction| {
            callback_processed.lock().expect("lock processed transactions").push(*transaction);
        })
        .wait();

        let mut processed = processed.lock().expect("lock processed transactions").clone();
        processed.sort_unstable();
        assert_eq!(processed, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn worker_panic_does_not_stop_later_candidates() {
        let executor = TaskExecutor::test();
        let (transactions, _) = TestBestTransactions::new(8);
        let processed = Arc::new(Mutex::new(Vec::new()));
        let callback_processed = Arc::clone(&processed);

        TransactionPrewarmer::start(&executor, transactions, move |transaction| {
            if *transaction == 1 {
                panic!("simulated speculative execution failure");
            }
            callback_processed.lock().expect("lock processed transactions").push(*transaction);
        })
        .wait();

        let mut processed = processed.lock().expect("lock processed transactions").clone();
        processed.sort_unstable();
        assert_eq!(processed, vec![0, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn dropping_job_waits_for_workers_and_releases_callback_resources() {
        let executor = TaskExecutor::test();
        let (transactions, _) = TestBestTransactions::new(1);
        let gate = Arc::new(Gate::default());
        let callback_gate = Arc::clone(&gate);
        let retained = Arc::new(());
        let callback_retained = Arc::clone(&retained);
        let (started_tx, started_rx) = mpsc::channel();

        let job = TransactionPrewarmer::start(&executor, transactions, move |_| {
            let _ = &callback_retained;
            started_tx.send(()).expect("report started worker");
            callback_gate.wait();
        });
        if started_rx.recv_timeout(TEST_TIMEOUT).is_err() {
            gate.open();
            drop(job);
            panic!("worker should start");
        }

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let drop_thread = thread::spawn(move || {
            drop(job);
            dropped_tx.send(()).expect("report job dropped");
        });
        let dropped_early = dropped_rx.recv_timeout(Duration::from_millis(50)).is_ok();

        gate.open();
        if !dropped_early {
            dropped_rx.recv_timeout(TEST_TIMEOUT).expect("job should finish dropping");
        }
        drop_thread.join().expect("join job drop thread");
        assert!(!dropped_early, "drop must wait for an in-flight worker");
        assert_eq!(Arc::strong_count(&retained), 1, "callback resources must be released");
    }

    #[test]
    fn speculative_execution_warms_account_code_and_storage_reads() {
        let fixture = Fixture::new(0);

        assert_eq!(fixture.context.prewarm_transactions([&fixture.transaction]), 1);

        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_account_with(fixture.sender, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(None))
        ));
        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_account_with(fixture.contract, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(Some(_)))
        ));
        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_code_with(fixture.code_hash, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(Some(_)))
        ));
        assert_eq!(
            fixture.cache.cache().get_or_try_insert_storage_with(
                fixture.contract,
                B256::ZERO,
                || Ok::<_, ()>(U256::ZERO),
            ),
            Ok(CachedStatus::Cached(U256::from(CONTRACT_STORAGE_VALUE)))
        );
    }

    #[test]
    fn speculative_execution_ignores_nonce_and_balance_checks() {
        let fixture = Fixture::new(7);

        assert_eq!(
            fixture.context.prewarm_transactions([&fixture.transaction]),
            1,
            "an unfunded sender with a future nonce should still execute for cache warming"
        );
    }

    #[test]
    fn speculative_execution_reports_transaction_failure() {
        let fixture = Fixture::with_transaction_chain_id(0, CHAIN_ID + 1);

        assert_eq!(
            fixture.context.prewarm_transactions([&fixture.transaction]),
            0,
            "a transaction invalid for the configured chain should not report successful execution"
        );
    }

    #[test]
    fn speculative_execution_does_not_commit_state_changes() {
        let fixture = Fixture::new(0);
        let original = fixture.client.basic_account(&fixture.sender).expect("read sender account");

        assert_eq!(fixture.context.prewarm_transactions([&fixture.transaction]), 1);
        assert_eq!(
            fixture.client.basic_account(&fixture.sender).expect("read sender account"),
            original,
            "speculative sender nonce and balance changes must be discarded"
        );
    }

    #[test]
    fn prewarm_transactions_counts_each_transaction_independently() {
        let fixture = Fixture::new(0);
        let invalid_transaction = Fixture::with_transaction_chain_id(0, CHAIN_ID + 1).transaction;

        assert_eq!(
            fixture.context.prewarm_transactions([&fixture.transaction, &invalid_transaction]),
            1,
            "only the valid transaction in the batch should count as successfully warmed"
        );
    }

    #[test]
    fn prewarm_transactions_skips_opening_state_provider_for_empty_batch() {
        let fixture = Fixture::new(0);
        let empty: [&BasePooledTransaction; 0] = [];

        assert_eq!(fixture.context.prewarm_transactions(empty), 0);
        assert_eq!(
            fixture.cache.usage_count(),
            2,
            "an empty batch must not open a state provider or touch the shared cache"
        );
    }

    #[test]
    fn dropping_context_releases_shared_cache_handle() {
        let fixture = Fixture::new(0);

        assert!(!fixture.cache.is_available(), "context must hold the shared cache");
        assert_eq!(
            fixture.cache.usage_count(),
            2,
            "only the saved cache and context should remain"
        );
        assert_eq!(fixture.context.prewarm_transactions([&fixture.transaction]), 1);
        assert_eq!(
            fixture.cache.usage_count(),
            2,
            "the batch's provider must release its cache handle after execution"
        );
        drop(fixture.context);
        assert!(fixture.cache.is_available(), "dropping context must release the shared cache");
    }
}
