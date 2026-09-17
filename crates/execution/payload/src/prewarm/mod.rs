//! Opt-in concurrent predicate-state prewarming for payload builds.
//!
//! A payload build reads declared validity-predicate state (account balances and
//! storage slots) for transactions ahead of the build loop. On a cold
//! [`ExecutionCache`] those reads hit the database under build-latency pressure. This
//! module warms that state concurrently and read-only into the very same cache the
//! build reads through, so first-touch reads become cache hits.
//!
//! # Structure
//!
//! * [`PrewarmWorkerPool`] — owned by the payload builder and shared by all of its
//!   builds. Spawns a fixed, bounded set of IO worker threads once, off the hot path.
//!   Each build gets one [`PrewarmJob`] dispatched to idle workers.
//! * [`PrewarmJob`] — one build's prewarming: it holds the build's
//!   [`PrewarmScheduler`] and, when dropped, cancels remaining queued work without
//!   waiting for worker IO.
//! * [`PrewarmScheduler`] — extracts, deduplicates, and caps the distinct state keys
//!   scheduled per build and owns the bounded work queue.
//! * [`WarmJob`] — the unit of work dispatched to workers: [`WarmJob::Key`] warms one
//!   declared predicate key, and [`WarmJob::Simulate`] warms a transaction's entire EVM
//!   read set by simulating it. Further warming modes extend this enum and the worker
//!   loop without changing queueing, scheduling, or cancellation.
//!
//! # Transaction-simulation warming
//!
//! [`WarmJob::Simulate`] runs the full EVM for a lookahead transaction against the
//! build's parent state, so accounts, storage slots, and bytecode the transaction touches
//! — not just its declared predicate keys — are pulled into the shared cache. It is a
//! strict opt-in on top of prewarming ([`PrewarmConfig::simulate`]).
//!
//! Simulation is **cache-warming only**. Results are discarded and the build loop always
//! re-executes every transaction: simulation never feeds inclusion, ordering, gas
//! accounting, or the payload. Safety rests on two properties:
//!
//! * Each simulation runs on a throwaway state overlay layered over the worker's
//!   cache-filling provider, so simulated writes are dropped with the overlay and never
//!   reach the canonical database. Only *reads* leak, into a read-through cache of
//!   committed parent state, so a cache hit returns exactly what the build's own read
//!   would have fetched.
//! * Sender-side gating (nonce, balance, base fee) is relaxed so a stale-nonce or
//!   underfunded lookahead transaction still exercises its call path and warms its reads.
//!   Because output is discarded, relaxation cannot affect the built block.
//!
//! Simulations run against a fixed parent snapshot, so a transaction whose relevant state
//! is changed by an earlier committed transaction may take a different path than the build
//! loop eventually takes and warm a different read set. That costs warming yield, never
//! correctness; `canonical_overtook_sim_total` and cache hit rate measure it.
//! * [`PrewarmingBestTransactions`] — a [`ParkablePayloadTransactions`] adapter owning
//!   the build's main transaction iterator untouched, plus an independent read-only
//!   lookahead cursor opened with the same attributes. It schedules at most an initial
//!   bounded burst plus one lookahead transaction per candidate the build consumes.
//!
//! # Invariants
//!
//! * The build's main iterator and its parking lifecycle (`park_current`,
//!   `mark_current_committed`, `promote`, `discard_parked`, `mark_invalid`) are
//!   delegated to the inner adapter unchanged. The lookahead cursor is a second,
//!   independent standard `best_transactions` iterator that is only ever advanced;
//!   it is never parked, promoted, invalidated, or committed.
//! * Scheduling never blocks the build loop: a full queue drops work instead of
//!   stalling, and dropping or finishing a build never waits on worker IO.
//! * Worker threads are bounded (one pool per builder, fixed size) and each worker
//!   constructs its own [`CachedStateProvider`] inside its thread (it is `!Sync`) for
//!   the job's exact parent state and cache, dropping both when the job ends. The
//!   engine skips execution-cache advancement while the shared cache has more than one
//!   handle, so this prompt release matters. Between cancellation and full cache
//!   availability there is an unavoidable tail of at most one in-flight blocking read
//!   per worker; queued work is dropped immediately on cancellation.
//! * Worker failures (provider open errors, failed reads, busy workers) are logged and
//!   counted; they never fail the build, and warm results are never used for
//!   correctness decisions.
//!
//! # Configuration and rollout
//!
//! [`PrewarmConfig`] is opt-in (`enabled: false` by default) and carries the IO worker
//! count, the bounded lookahead, and the per-build distinct-key cap. When the builder
//! flag is enabled, the node bins also enable the engine's
//! `share_execution_cache_with_payload_builder` so builds actually receive the shared
//! cache; without a shared cache no prewarming runs regardless of configuration.
//! `--builder.prewarm-workers` budgets an additional pool; account for the engine's
//! existing prewarming workers when sizing total database read concurrency.

use std::sync::Arc;

mod config;
pub use config::PrewarmConfig;

mod keys;
pub use keys::{KeyQueue, KeyQueueState, WarmKey};

mod sim;
pub use sim::{SimJob, SimJobFactory, SimSchedulerState, SimSetup, SimulateFn};

mod scheduler;
pub use scheduler::{JobCompletion, PrewarmScheduler};

mod pool;
pub use pool::{PrewarmJob, PrewarmWorkerPool, WorkerJob, WorkerLease};

mod cursor;
pub use cursor::PrewarmingBestTransactions;

/// Where prewarm workers log and count.
const TARGET: &str = "payload_builder::prewarm";

/// A unit of prewarm work dispatched to an IO worker.
///
/// This is the extension seam for warming modes: variants slot into the same bounded
/// queue and worker loop without changing the scheduling, cancellation, or cache-release
/// structure.
#[derive(Debug)]
pub enum WarmJob {
    /// Warm the state read by one declared predicate key.
    Key(WarmKey),
    /// Warm a transaction's full read set by simulating it.
    Simulate(Arc<SimJob>),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc::sync_channel,
        },
        thread,
        time::{Duration, Instant},
    };

    use alloy_consensus::{SignableTransaction, Transaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, Signature, StorageKey, TxHash, TxKind, U256};
    use base_common_consensus::BasePooledTransaction as ConsensusPooledTransaction;
    use base_execution_txpool::{BasePooledTransaction, ValidityOperator, ValidityPredicate};
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;
    use reth_execution_cache::{CachedStateProvider, ExecutionCache};
    use reth_payload_util::PayloadTransactions;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::{
        AccountReader, BlockHashReader, StateProvider, StateProviderBox, errors::ProviderResult,
    };
    use reth_transaction_pool::{
        BestTransactions, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
        error::InvalidPoolTransactionError, identifier::TransactionId,
    };

    use super::*;
    use crate::ParkablePayloadTransactions;

    fn enabled_config(workers: usize, lookahead: usize, key_cap: usize) -> PrewarmConfig {
        PrewarmConfig {
            enabled: true,
            worker_count: workers,
            lookahead,
            key_cap,
            ..PrewarmConfig::default()
        }
    }

    fn sim_config(workers: usize, lookahead: usize, sim_lookahead: usize) -> PrewarmConfig {
        PrewarmConfig {
            enabled: true,
            worker_count: workers,
            lookahead,
            key_cap: 64,
            simulate: true,
            sim_lookahead,
        }
    }

    /// A simulation job standing in for the builder-supplied EVM closure: it performs the
    /// account, storage, and bytecode reads a real `transact` would issue through the
    /// worker's cache-filling provider.
    fn reading_sim_job(
        tx_hash: TxHash,
        address: Address,
        slot: U256,
    ) -> (SimJob, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let job = SimJob {
            tx_hash,
            simulate: {
                let runs = Arc::clone(&runs);
                Box::new(move |provider| {
                    runs.fetch_add(1, Ordering::Relaxed);
                    let _ = provider.basic_account(&address);
                    let _ = provider.storage(address, StorageKey::new(slot.to_be_bytes()));
                })
            },
        };
        (job, runs)
    }

    /// A no-op simulation job, for scheduling-only assertions.
    fn noop_sim_job(tx_hash: TxHash) -> SimJob {
        SimJob { tx_hash, simulate: Box::new(|_| {}) }
    }

    /// A simulation setup recording every transaction the adapter asks it to simulate.
    fn recording_sim_setup(
        lookahead: usize,
    ) -> (SimSetup<BasePooledTransaction>, Arc<Mutex<Vec<TxHash>>>) {
        let requested = Arc::new(Mutex::new(Vec::new()));
        let setup = SimSetup {
            lookahead,
            factory: {
                let requested = Arc::clone(&requested);
                Arc::new(move |transaction: &BasePooledTransaction| {
                    let tx_hash = *transaction.hash();
                    requested.lock().unwrap().push(tx_hash);
                    Some(noop_sim_job(tx_hash))
                })
            },
        };
        (setup, requested)
    }

    fn balance_predicate(address: Address) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op: ValidityOperator::LessThan, value: U256::from(1) }
    }

    fn storage_predicate(address: Address, slot: U256) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot,
            mask: ValidityPredicate::default_mask(),
            op: ValidityOperator::LessThan,
            value: U256::from(1),
        }
    }

    fn validity_transaction(
        sender: Address,
        predicates: Vec<ValidityPredicate>,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(sender),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signed = tx.into_signed(Signature::new(U256::from(1), U256::from(1), false));
        let pooled = ConsensusPooledTransaction::Legacy(signed);
        let encoded_length = pooled.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(pooled.into(), sender),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), 0),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    /// Read-only state provider stub over a [`MockEthProvider`] that delays and counts
    /// account and storage reads (the reads prewarming is meant to remove).
    #[derive(Debug, Clone)]
    struct DelayStateProvider {
        inner: MockEthProvider,
        delay: Duration,
        account_reads: Arc<AtomicUsize>,
        storage_reads: Arc<AtomicUsize>,
    }

    impl DelayStateProvider {
        fn new(inner: MockEthProvider, delay: Duration) -> Self {
            Self {
                inner,
                delay,
                account_reads: Arc::new(AtomicUsize::new(0)),
                storage_reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn account_reads(&self) -> usize {
            self.account_reads.load(Ordering::Relaxed)
        }

        fn storage_reads(&self) -> usize {
            self.storage_reads.load(Ordering::Relaxed)
        }

        fn delay_once(&self) {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
        }
    }

    impl reth_storage_api::AccountReader for DelayStateProvider {
        fn basic_account(
            &self,
            address: &Address,
        ) -> reth_storage_api::errors::ProviderResult<Option<reth_primitives_traits::Account>>
        {
            self.account_reads.fetch_add(1, Ordering::Relaxed);
            self.delay_once();
            self.inner.basic_account(address)
        }
    }

    impl BlockHashReader for DelayStateProvider {
        fn block_hash(
            &self,
            number: alloy_primitives::BlockNumber,
        ) -> reth_storage_api::errors::ProviderResult<Option<B256>> {
            self.inner.block_hash(number)
        }

        fn canonical_hashes_range(
            &self,
            start: alloy_primitives::BlockNumber,
            end: alloy_primitives::BlockNumber,
        ) -> reth_storage_api::errors::ProviderResult<Vec<B256>> {
            self.inner.canonical_hashes_range(start, end)
        }
    }

    impl reth_storage_api::BytecodeReader for DelayStateProvider {
        fn bytecode_by_hash(
            &self,
            code_hash: &B256,
        ) -> reth_storage_api::errors::ProviderResult<Option<reth_primitives_traits::Bytecode>>
        {
            self.inner.bytecode_by_hash(code_hash)
        }
    }

    impl reth_storage_api::StateRootProvider for DelayStateProvider {
        fn state_root(
            &self,
            hashed_state: reth_trie_common::HashedPostState,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.state_root(hashed_state)
        }

        fn state_root_from_nodes(
            &self,
            input: reth_trie_common::TrieInput,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.state_root_from_nodes(input)
        }

        fn state_root_with_updates(
            &self,
            hashed_state: reth_trie_common::HashedPostState,
        ) -> reth_storage_api::errors::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>
        {
            self.inner.state_root_with_updates(hashed_state)
        }

        fn state_root_from_nodes_with_updates(
            &self,
            input: reth_trie_common::TrieInput,
        ) -> reth_storage_api::errors::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>
        {
            self.inner.state_root_from_nodes_with_updates(input)
        }
    }

    impl reth_storage_api::StorageRootProvider for DelayStateProvider {
        fn storage_root(
            &self,
            address: Address,
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.storage_root(address, hashed_storage)
        }

        fn storage_proof(
            &self,
            address: Address,
            slot: B256,
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::StorageProof> {
            self.inner.storage_proof(address, slot, hashed_storage)
        }

        fn storage_multiproof(
            &self,
            address: Address,
            slots: &[B256],
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::StorageMultiProof> {
            self.inner.storage_multiproof(address, slots, hashed_storage)
        }
    }

    impl reth_storage_api::StateProofProvider for DelayStateProvider {
        fn proof(
            &self,
            input: reth_trie_common::TrieInput,
            address: Address,
            slots: &[B256],
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::AccountProof> {
            self.inner.proof(input, address, slots)
        }

        fn multiproof(
            &self,
            input: reth_trie_common::TrieInput,
            targets: reth_trie_common::MultiProofTargets,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::MultiProof> {
            self.inner.multiproof(input, targets)
        }

        fn witness(
            &self,
            input: reth_trie_common::TrieInput,
            target: reth_trie_common::HashedPostState,
            mode: reth_trie_common::ExecutionWitnessMode,
        ) -> reth_storage_api::errors::ProviderResult<Vec<alloy_primitives::Bytes>> {
            self.inner.witness(input, target, mode)
        }
    }

    impl reth_storage_api::HashedPostStateProvider for DelayStateProvider {
        fn hashed_post_state(
            &self,
            bundle_state: &revm::database::BundleState,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::HashedPostState> {
            self.inner.hashed_post_state(bundle_state)
        }
    }

    impl StateProvider for DelayStateProvider {
        fn storage(
            &self,
            account: Address,
            storage_key: StorageKey,
        ) -> reth_storage_api::errors::ProviderResult<Option<alloy_primitives::StorageValue>>
        {
            self.storage_reads.fetch_add(1, Ordering::Relaxed);
            self.delay_once();
            self.inner.storage(account, storage_key)
        }
    }

    /// Builds a prewarm pool plus one shared cache, and a provider factory that opens a
    /// delaying provider over `mock`.
    fn test_pool(
        config: &PrewarmConfig,
        mock: &MockEthProvider,
        delay: Duration,
    ) -> (
        PrewarmWorkerPool,
        ExecutionCache,
        impl Fn() -> ProviderResult<StateProviderBox> + Clone + Send + 'static,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let provider = DelayStateProvider::new(mock.clone(), delay);
        let account_reads = Arc::clone(&provider.account_reads);
        let storage_reads = Arc::clone(&provider.storage_reads);
        let pool = PrewarmWorkerPool::new(config);
        let cache = ExecutionCache::new(1000);
        let factory = move || Ok(Box::new(provider.clone()) as StateProviderBox);
        (pool, cache, factory, account_reads, storage_reads)
    }

    fn warmable_account(mock: &MockEthProvider, address: Address) -> StorageKey {
        let storage_key = StorageKey::new(U256::from(7).to_be_bytes());
        mock.extend_accounts(vec![(
            address,
            ExtendedAccount::new(1, U256::from(100))
                .extend_storage(vec![(storage_key, U256::from(42))]),
        )]);
        storage_key
    }

    fn wait_until(condition: impl Fn() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[test]
    fn warm_keys_come_from_state_predicates_only() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3);
        let predicates = [
            balance_predicate(address),
            storage_predicate(address, slot),
            ValidityPredicate::BlockNumber {
                op: base_execution_txpool::ValidityOperator::Equal,
                value: U256::from(10),
            },
            ValidityPredicate::FlashblockIndex {
                op: base_execution_txpool::ValidityOperator::Equal,
                value: U256::from(1),
            },
        ];

        let keys = WarmKey::for_predicates(&predicates).collect::<Vec<_>>();
        assert_eq!(keys, vec![WarmKey::Balance(address), WarmKey::Storage(address, slot)]);
        assert_eq!(WarmKey::from_predicate(&predicates[2]), None);
    }

    #[test]
    fn queue_push_is_bounded_and_nonblocking() {
        let queue = KeyQueue::new(2);
        assert!(queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(1)))));
        assert!(queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(2)))));
        assert_eq!(queue.len(), 2);
        // Full queue drops instead of blocking; a closed queue refuses everything.
        assert!(!queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(3)))));
        queue.close();
        assert!(queue.is_closed());
        assert!(!queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(4)))));
    }

    #[test]
    fn queue_close_wakes_blocked_pop_and_drops_queued_work() {
        let queue = Arc::new(KeyQueue::new(4));
        queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(1))));
        queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(2))));

        let popped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handle = {
            let queue = Arc::clone(&queue);
            let popped = Arc::clone(&popped);
            thread::spawn(move || {
                while let Some(job) = queue.pop() {
                    popped.lock().unwrap().push(job);
                }
            })
        };
        assert!(wait_until(|| popped.lock().unwrap().len() == 2, Duration::from_secs(5)));
        queue.close();
        handle.join().expect("blocked pop must wake on close");

        assert_eq!(popped.lock().unwrap().len(), 2);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn scheduler_dedups_and_saturates_at_key_cap() {
        let address = Address::with_last_byte(1);
        let scheduler = PrewarmScheduler::new(&enabled_config(1, 8, 2));

        assert!(scheduler.schedule_key(WarmKey::Balance(address)));
        assert!(scheduler.schedule_key(WarmKey::Balance(address)), "duplicates count as scheduled");
        assert_eq!(scheduler.queued_len(), 1, "duplicate key must not be queued twice");

        assert!(scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(2))));
        assert!(!scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(3))));
        assert!(scheduler.is_saturated(), "key cap must saturate the scheduler");
        assert_eq!(scheduler.queued_len(), 2);
    }

    #[test]
    fn disabled_pool_spawns_no_workers_and_starts_no_jobs() {
        let pool = PrewarmWorkerPool::new(&PrewarmConfig::default());
        assert_eq!(pool.worker_count(), 0);
        assert!(
            pool.try_start_job(
                || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
                ExecutionCache::new(1000)
            )
            .is_none()
        );
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn disconnected_worker_is_counted_and_releases_job() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let busy = Arc::new(AtomicBool::new(false));
        let pool = PrewarmWorkerPool {
            config: enabled_config(1, 8, 64),
            workers: vec![(sender, Arc::clone(&busy))],
        };
        let cache = reth_execution_cache::SavedCache::new(B256::ZERO, ExecutionCache::new(1000));
        metrics::with_local_recorder(&recorder, || {
            assert!(
                pool.try_start_job(
                    || panic!("a disconnected worker must not open a provider"),
                    cache.cache().clone(),
                )
                .is_none()
            );
        });
        assert!(!busy.load(Ordering::Acquire));
        assert!(cache.is_available());
        let output = handle.render();
        assert!(
            output.lines().any(|line| line == "base_payload_prewarm_worker_disconnected_total 1")
        );
        assert!(
            !output.lines().any(|line| line == "base_payload_prewarm_worker_busy_skips_total 1")
        );
    }

    fn slots_cached(
        cache: &ExecutionCache,
        address: Address,
        slots: impl Iterator<Item = usize>,
    ) -> bool {
        slots.into_iter().all(|slot| {
            cache
                .get_or_try_insert_storage_with(
                    address,
                    StorageKey::new(U256::from(slot).to_be_bytes()),
                    || Err::<U256, ()>(()),
                )
                .is_ok()
        })
    }

    #[test]
    fn busy_workers_skip_new_jobs_and_partial_dispatch_completes() {
        let pool = PrewarmWorkerPool::new(&enabled_config(2, 8, 64));
        let opened = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(std::sync::Barrier::new(2));
        let first = pool
            .try_start_job(
                {
                    let opened = Arc::clone(&opened);
                    let release = Arc::clone(&release);
                    move || {
                        if opened.fetch_add(1, Ordering::Relaxed) == 0 {
                            return Err(
                                reth_storage_api::errors::ProviderError::FinalizedBlockNotFound,
                            );
                        }
                        release.wait();
                        Ok(Box::new(MockEthProvider::default()) as StateProviderBox)
                    }
                },
                ExecutionCache::new(1000),
            )
            .unwrap();
        assert!(wait_until(
            || opened.load(Ordering::Relaxed) == 2
                && *first.completion.remaining.lock().unwrap() == 1,
            Duration::from_secs(5)
        ));
        let completion = Arc::clone(&first.completion);
        drop(first);

        // Only the worker whose first job failed is available. The blocked worker
        // must not retain a queued job (and its different-parent cache).
        let cache =
            reth_execution_cache::SavedCache::new(B256::repeat_byte(2), ExecutionCache::new(1000));
        let second = pool
            .try_start_job(
                || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
                cache.cache().clone(),
            )
            .unwrap();
        let second_done = Arc::clone(&second.completion);
        drop(second);
        assert!(wait_until(|| *second_done.remaining.lock().unwrap() == 0, Duration::from_secs(5)));
        assert!(cache.is_available());
        release.wait();
        completion.wait();
    }

    #[test]
    fn prewarm_fills_shared_cache_and_eliminates_backend_reads() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(9);
        warmable_account(&mock, address);

        let (pool, cache, factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, Duration::ZERO);
        let job = pool.try_start_job(factory, cache.clone()).expect("worker must take the job");
        assert!(job.scheduler().schedule_key(WarmKey::Balance(address)));
        assert!(job.scheduler().schedule_key(WarmKey::Storage(address, U256::from(7))));
        assert!(wait_until(
            || slots_cached(&cache, address, std::iter::once(7)),
            Duration::from_secs(5)
        ));
        job.join();

        // A build-side provider over a different (empty) backend must be served entirely
        // from the warmed shared cache, mirroring the build loop's revm read path.
        let build_backend = DelayStateProvider::new(MockEthProvider::default(), Duration::ZERO);
        let build_provider =
            CachedStateProvider::new(Box::new(build_backend.clone()), cache.clone(), None);
        let mut db = reth_revm::database::StateProviderDatabase::new(build_provider);
        let account = revm::Database::basic(&mut db, address).unwrap().expect("warmed account");
        assert_eq!(account.balance, U256::from(100));
        let value = revm::Database::storage(&mut db, address, U256::from(7)).unwrap();
        assert_eq!(value, U256::from(42));
        assert_eq!(build_backend.account_reads(), 0, "warmed account must not hit the backend");
        assert_eq!(build_backend.storage_reads(), 0, "warmed slot must not hit the backend");

        drop(db);
        // Workers release their cache handles when the job ends: only this handle remains.
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(saved.is_available());
    }

    #[test]
    fn prewarm_tolerates_missing_parent_state() {
        let (pool, cache, _factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &MockEthProvider::default(), Duration::ZERO);
        let job = pool
            .try_start_job(
                || Err(reth_storage_api::errors::ProviderError::FinalizedBlockNotFound),
                cache.clone(),
            )
            .expect("worker must take the job");
        job.join();
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(saved.is_available(), "worker must release the cache after the error");
    }

    #[test]
    fn prewarm_cancellation_drops_queued_work_and_releases_cache() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(5);
        warmable_account(&mock, address);
        let delay = Duration::from_millis(250);
        let (pool, cache, factory, _account_reads, storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, delay);
        let job = pool.try_start_job(factory, cache.clone()).expect("worker must take the job");
        for index in 0..3 {
            assert!(job.scheduler().schedule_key(WarmKey::Storage(address, U256::from(index))));
        }

        // Wait until the first (blocked) read is in flight, then cancel the job.
        assert!(wait_until(|| storage_reads.load(Ordering::Relaxed) == 1, Duration::from_secs(2)));
        let drop_start = Instant::now();
        drop(job);
        assert!(
            drop_start.elapsed() < delay / 2,
            "dropping the job must not wait for the in-flight read"
        );

        // The in-flight read finishes, and the worker then releases its cache handle and
        // drops the remaining queued keys.
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(wait_until(|| saved.is_available(), Duration::from_secs(5)));
        assert_eq!(
            storage_reads.load(Ordering::Relaxed),
            1,
            "queued work must be cancelled, not warmed"
        );
    }

    #[test]
    fn prewarm_thread_bound_holds_across_dropped_jobs_with_blocked_io() {
        let (pool, cache, _factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(2, 8, 64), &MockEthProvider::default(), Duration::ZERO);
        assert_eq!(pool.worker_count(), 2);

        // A factory whose provider open blocks: each dispatch ties up workers while the
        // build drops its job immediately, the rapid-cancellation pattern.
        let blocked = || {
            std::thread::sleep(Duration::from_millis(150));
            Ok(Box::new(DelayStateProvider::new(MockEthProvider::default(), Duration::ZERO))
                as StateProviderBox)
        };
        for _ in 0..6 {
            if let Some(job) = pool.try_start_job(blocked, cache.clone()) {
                drop(job);
            }
        }
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);

        // Every blocked open finishes, every worker releases its cache, and the pool
        // still serves a new build with its fixed threads (no accumulation).
        assert!(wait_until(|| saved.is_available(), Duration::from_secs(10)));
        let final_job = pool
            .try_start_job(
                || {
                    Ok(
                        Box::new(DelayStateProvider::new(
                            MockEthProvider::default(),
                            Duration::ZERO,
                        )) as StateProviderBox,
                    )
                },
                saved.cache().clone(),
            )
            .expect("pool must still accept jobs after repeated cancellations");
        final_job.scheduler().schedule_key(WarmKey::Balance(Address::with_last_byte(1)));
        final_job.join();
        assert!(saved.is_available());
    }

    /// Vec-backed lookahead cursor double.
    struct StaticCursor {
        transactions: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    }

    impl StaticCursor {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl Iterator for StaticCursor {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl BestTransactions for StaticCursor {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: InvalidPoolTransactionError) {}

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    /// Inner adapter double recording the parking lifecycle calls it receives.
    struct SpyInner {
        transactions: VecDeque<BasePooledTransaction>,
        parked: usize,
        committed: usize,
        promoted: Vec<TxHash>,
        discarded: Vec<TxHash>,
        invalidated: usize,
    }

    impl SpyInner {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self {
                transactions: transactions
                    .into_iter()
                    .map(|transaction| transaction.transaction.clone())
                    .collect(),
                parked: 0,
                committed: 0,
                promoted: Vec::new(),
                discarded: Vec::new(),
                invalidated: 0,
            }
        }
    }

    impl PayloadTransactions for SpyInner {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            self.transactions.pop_front()
        }

        fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {
            self.invalidated += 1;
        }
    }

    impl ParkablePayloadTransactions for SpyInner {
        fn park_current(&mut self) -> bool {
            self.parked += 1;
            true
        }

        fn mark_current_committed(&mut self) {
            self.committed += 1;
        }

        fn promote(&mut self, transaction_hash: TxHash) -> bool {
            self.promoted.push(transaction_hash);
            true
        }

        fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
            self.discarded.push(transaction_hash);
            true
        }
    }

    #[test]
    fn adapter_schedules_initial_burst_then_one_advance_per_candidate() {
        let senders: Vec<Address> =
            (0..4).map(|index| Address::with_last_byte(index + 1)).collect();
        let transactions: Vec<_> = senders
            .iter()
            .map(|sender| {
                validity_transaction(
                    *sender,
                    vec![balance_predicate(*sender), storage_predicate(*sender, U256::from(7))],
                )
            })
            .collect();

        let scheduler = Arc::new(PrewarmScheduler::new(&enabled_config(1, 2, 16)));
        let mut adapter =
            PrewarmingBestTransactions::<SpyInner, BasePooledTransaction>::with_cursor(
                SpyInner::new(transactions.clone()),
                Some(Box::new(StaticCursor::new(transactions))),
                Some(Arc::clone(&scheduler)),
                None,
            );

        // Initial bounded burst: lookahead transactions' keys are queued before the loop.
        assert_eq!(scheduler.queued_len(), 4, "two lookahead transactions, two keys each");

        // Each consumed candidate schedules exactly one further lookahead transaction.
        adapter.next(()).expect("candidate");
        assert_eq!(scheduler.queued_len(), 6);

        // The parking lifecycle is delegated to the inner adapter untouched.
        assert!(adapter.park_current());
        assert_eq!(adapter.inner.parked, 1);

        let second = adapter.next(()).expect("candidate");
        assert_eq!(scheduler.queued_len(), 8);

        adapter.mark_current_committed();
        assert_eq!(adapter.inner.committed, 1);

        let hash = *second.hash();
        assert!(adapter.promote(hash));
        assert!(adapter.discard_parked(hash));
        assert_eq!(adapter.inner.promoted, vec![hash]);
        assert_eq!(adapter.inner.discarded, vec![hash]);

        // The lookahead cursor stops at the bounded schedule: exhausting it schedules no
        // further keys.
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_none());
        assert_eq!(scheduler.queued_len(), 8, "cursor exhausted: no further scheduling");

        adapter.mark_invalid(second.sender(), second.nonce());
        assert_eq!(adapter.inner.invalidated, 1);
    }

    #[test]
    fn simulation_job_warms_its_reads_into_the_shared_cache() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(11);
        warmable_account(&mock, address);

        let config = sim_config(1, 8, 4);
        let (pool, cache, factory, _account_reads, _storage_reads) =
            test_pool(&config, &mock, Duration::ZERO);
        let job = pool.try_start_job(factory, cache.clone()).expect("worker must take the job");

        let tx_hash = TxHash::with_last_byte(1);
        let (sim, runs) = reading_sim_job(tx_hash, address, U256::from(7));
        assert!(job.scheduler().schedule_simulation(sim));
        // The simulation is in flight until a worker finishes it.
        assert!(
            wait_until(
                || runs.load(Ordering::Relaxed) == 1
                    && !job.scheduler().simulation_pending(&tx_hash),
                Duration::from_secs(5)
            ),
            "the worker must run the simulation and then mark it finished"
        );
        job.join();

        // The simulated reads are in the shared cache: a build-side provider over an empty
        // backend is served entirely from it, exactly as for predicate-key warming.
        let build_backend = DelayStateProvider::new(MockEthProvider::default(), Duration::ZERO);
        let build_provider =
            CachedStateProvider::new(Box::new(build_backend.clone()), cache.clone(), None);
        let mut db = reth_revm::database::StateProviderDatabase::new(build_provider);
        let account = revm::Database::basic(&mut db, address).unwrap().expect("warmed account");
        assert_eq!(account.balance, U256::from(100));
        assert_eq!(
            revm::Database::storage(&mut db, address, U256::from(7)).unwrap(),
            U256::from(42)
        );
        assert_eq!(build_backend.account_reads(), 0, "warmed account must not hit the backend");
        assert_eq!(build_backend.storage_reads(), 0, "warmed slot must not hit the backend");

        drop(db);
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(saved.is_available(), "the worker must release its cache handle");
    }

    #[test]
    fn sim_scheduler_dedups_simulations() {
        let scheduler = PrewarmScheduler::new(&sim_config(1, 8, 4));
        let first = TxHash::with_last_byte(1);

        assert!(scheduler.schedule_simulation(noop_sim_job(first)));
        assert!(
            scheduler.schedule_simulation(noop_sim_job(first)),
            "duplicates count as scheduled"
        );
        assert_eq!(scheduler.queued_len(), 1, "a duplicate must not be queued twice");

        assert!(scheduler.schedule_simulation(noop_sim_job(TxHash::with_last_byte(2))));
        assert_eq!(scheduler.queued_len(), 2, "distinct simulations are each queued once");

        // Predicate-key warming has its own budget and is unaffected.
        assert!(!scheduler.is_saturated());
        assert!(scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(1))));
    }

    #[test]
    fn simulation_is_refused_unless_configured() {
        let scheduler = PrewarmScheduler::new(&enabled_config(1, 8, 64));
        assert!(scheduler.is_sim_saturated(), "simulation is off by default");
        assert!(!scheduler.schedule_simulation(noop_sim_job(TxHash::with_last_byte(1))));
        assert_eq!(scheduler.queued_len(), 0);
        // A closed queue refuses simulation jobs too.
        let scheduler = PrewarmScheduler::new(&sim_config(1, 8, 4));
        scheduler.close();
        assert!(!scheduler.schedule_simulation(noop_sim_job(TxHash::with_last_byte(1))));
    }

    #[test]
    fn adapter_simulates_only_non_predicated_transactions_in_a_bounded_window() {
        let predicated = Address::with_last_byte(1);
        let transactions = vec![
            // Predicated: warmed by declared keys, never simulated.
            validity_transaction(predicated, vec![balance_predicate(predicated)]),
            validity_transaction(Address::with_last_byte(2), vec![]),
            validity_transaction(Address::with_last_byte(3), vec![]),
            validity_transaction(Address::with_last_byte(4), vec![]),
        ];
        let expected_first = *transactions[1].transaction.hash();
        let expected_second = *transactions[2].transaction.hash();
        let expected_third = *transactions[3].transaction.hash();

        // A simulation window of one: only one simulation may be outstanding, so the rest
        // wait for a worker to finish rather than being lost to the shared cursor.
        let scheduler = Arc::new(PrewarmScheduler::new(&sim_config(1, 4, 1)));
        let (sim, requested) = recording_sim_setup(1);
        let mut adapter =
            PrewarmingBestTransactions::<SpyInner, BasePooledTransaction>::with_cursor(
                SpyInner::new(transactions.clone()),
                Some(Box::new(StaticCursor::new(transactions))),
                Some(Arc::clone(&scheduler)),
                Some(sim),
            );

        // The predicated transaction is skipped; the window admits exactly one simulation.
        assert_eq!(
            requested.lock().unwrap().as_slice(),
            &[expected_first],
            "only the first non-predicated transaction is simulated"
        );
        assert_eq!(
            adapter.sim_deferred.len(),
            2,
            "the cursor ran ahead of the window; the rest wait instead of being dropped"
        );

        // No worker is running here, so the window only reopens as simulations finish.
        scheduler.finish_simulation(&expected_first);
        adapter.next(()).expect("candidate");
        assert_eq!(requested.lock().unwrap().len(), 2);
        assert_eq!(requested.lock().unwrap()[1], expected_second, "nearest the build loop first");

        scheduler.finish_simulation(&expected_second);
        adapter.next(()).expect("candidate");
        assert_eq!(requested.lock().unwrap().len(), 3);
        assert_eq!(requested.lock().unwrap()[2], expected_third);
        assert!(adapter.sim_deferred.is_empty(), "every eligible transaction was simulated");

        // The predicated transaction still had its declared key warmed.
        assert!(scheduler.queued_len() >= 1);
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn build_loop_overtaking_an_unfinished_simulation_is_counted() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let transactions = vec![validity_transaction(Address::with_last_byte(2), vec![])];

        let scheduler = Arc::new(PrewarmScheduler::new(&sim_config(1, 4, 4)));
        let (sim, _requested) = recording_sim_setup(4);
        metrics::with_local_recorder(&recorder, || {
            let mut adapter =
                PrewarmingBestTransactions::<SpyInner, BasePooledTransaction>::with_cursor(
                    SpyInner::new(transactions.clone()),
                    Some(Box::new(StaticCursor::new(transactions))),
                    Some(Arc::clone(&scheduler)),
                    Some(sim),
                );
            // No worker is running, so the scheduled simulation is still pending when the
            // build loop reaches the same transaction.
            adapter.next(()).expect("candidate");
        });

        assert!(
            handle
                .render()
                .lines()
                .any(|line| line == "base_payload_prewarm_canonical_overtook_sim_total 1"),
            "reaching a transaction whose simulation is still pending must be counted"
        );
    }
}
