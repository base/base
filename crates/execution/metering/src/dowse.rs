//! Deterministic Dowse state-prefetch benchmarks for canonical block replay.

use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{
        Arc, Barrier, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::B256;
use base_builder_core::{DowsePrefetchCache, DowsePrefetchTarget};
use base_common_consensus::BaseBlock;
use base_execution_chainspec::BaseChainSpec;
use dowse_plan::{PlanLimits, PrefetchPlanner};
use dowse_types::HintTable;
use eyre::{Result as EyreResult, eyre};
use reth_execution_cache::{CachedStatus, ExecutionCache};
use reth_primitives_traits::Block as BlockT;
use reth_provider::{HeaderProvider, StateProvider, StateProviderFactory};

use crate::{
    DowseBlockBenchmarkResponse, DowseBlockReplayResponse, DowseConcurrentBlockReplayResponse,
    DowseConcurrentPrefetchStats, DowseConcurrentReplayConfig, DowsePrefetchReadCounts,
    DowsePrefetchStats, meter_block, meter_block_with_cache_callbacks,
    meter_block_with_optional_cache,
};

/// Static hint table and worker settings for block replay benchmarks.
#[derive(Clone, Debug)]
pub struct DowseBenchmarkConfig {
    hints: Arc<HintTable>,
    worker_count: usize,
    cache_size_bytes: usize,
}

impl DowseBenchmarkConfig {
    /// Loads a JSON hint table and validates benchmark resource settings.
    pub fn load(
        hints_path: impl AsRef<Path>,
        worker_count: usize,
        cache_size_bytes: usize,
    ) -> EyreResult<Self> {
        let path = hints_path.as_ref();
        let bytes = fs::read(path).map_err(|error| {
            eyre::eyre!("failed to read Dowse benchmark hints at {}: {error}", path.display())
        })?;
        let hints: HintTable = serde_json::from_slice(&bytes).map_err(|error| {
            eyre::eyre!("failed to parse Dowse benchmark hints at {}: {error}", path.display())
        })?;
        eyre::ensure!(hints.version == 1, "unsupported Dowse hint table version {}", hints.version);
        eyre::ensure!(worker_count > 0, "Dowse benchmark worker count must be greater than zero");
        eyre::ensure!(cache_size_bytes > 0, "Dowse benchmark cache size must be greater than zero");

        Ok(Self { hints: Arc::new(hints), worker_count, cache_size_bytes })
    }

    /// Replays a block while ordered prefetch workers race EVM execution after a finite head start.
    pub fn replay_concurrent_block<P>(
        &self,
        provider: P,
        chain_spec: Arc<BaseChainSpec>,
        block: &BaseBlock,
        replay_config: DowseConcurrentReplayConfig,
    ) -> EyreResult<DowseConcurrentBlockReplayResponse>
    where
        P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + Clone + Sync,
    {
        eyre::ensure!(replay_config.workers > 0, "Dowse replay workers must be greater than zero");
        eyre::ensure!(replay_config.workers <= 256, "Dowse replay workers must not exceed 256");
        eyre::ensure!(
            replay_config.locality_batch_size > 0,
            "Dowse replay locality batch size must be greater than zero"
        );
        eyre::ensure!(
            replay_config.min_confidence_bps <= 10_000,
            "Dowse replay minimum confidence must not exceed 10000 basis points"
        );

        let planning_start = Instant::now();
        let planner = PrefetchPlanner::new(
            &self.hints,
            PlanLimits::new(
                replay_config.max_accounts_per_transaction,
                replay_config.max_storage_slots_per_transaction,
            ),
        );
        let mut seen_accounts = HashSet::new();
        let mut seen_storage = HashSet::new();
        let mut planned_transactions = 0;
        let parent_hash = block.header().parent_hash;
        let scheduler = DowsePrefetchCache::new(
            usize::MAX,
            replay_config.max_transaction_distance,
            replay_config.min_confidence_bps,
        );
        let cache = scheduler.activate(parent_hash, self.cache_size_bytes);

        for (rank, transaction) in block.body().transactions().enumerate() {
            let tx_hash = B256::from(*transaction.tx_hash());
            scheduler.observe_transaction_rank(parent_hash, tx_hash, rank);
            let Some(target) = transaction.to() else {
                continue;
            };
            let sender = transaction
                .recover_signer()
                .map_err(|error| eyre!("failed to recover transaction signer: {error}"))?;
            let Some(plan) = planner.plan(target, sender, transaction.input()) else {
                continue;
            };
            if plan.target_count() == 0 {
                continue;
            }

            let mut admitted = false;
            for (address, confidence) in plan
                .accounts
                .iter()
                .copied()
                .zip(plan.account_confidence.iter().copied().chain(std::iter::repeat(1.0)))
            {
                if DowsePrefetchCache::confidence_bps(confidence)
                    >= replay_config.min_confidence_bps
                {
                    admitted = true;
                    seen_accounts.insert(address);
                }
            }
            for (target, confidence) in plan
                .storage
                .iter()
                .copied()
                .zip(plan.storage_confidence.iter().copied().chain(std::iter::repeat(1.0)))
            {
                if DowsePrefetchCache::confidence_bps(confidence)
                    >= replay_config.min_confidence_bps
                {
                    admitted = true;
                    seen_storage.insert(target);
                }
            }
            if admitted {
                planned_transactions += 1;
            }
            scheduler.submit_plan(parent_hash, tx_hash, rank, plan);
        }

        let planning_time_us = planning_start.elapsed().as_micros();
        let account_targets = seen_accounts.len();
        let storage_targets = seen_storage.len();
        let workers = replay_config.workers.min(account_targets + storage_targets);

        if workers == 0 {
            let replay =
                meter_block_with_optional_cache(provider, chain_spec, block, Some(cache), || {})?;
            return Ok(DowseConcurrentBlockReplayResponse {
                config: replay_config,
                prefetch: DowseConcurrentPrefetchStats {
                    planning_time_us,
                    prefetch_time_us: 0,
                    actual_head_start_us: 0,
                    planned_transactions,
                    account_targets,
                    storage_targets,
                    workers,
                    completed_before_execution: DowsePrefetchReadCounts::default(),
                    completed_during_execution: DowsePrefetchReadCounts::default(),
                    completed_after_execution: DowsePrefetchReadCounts::default(),
                    cache_hits: DowsePrefetchReadCounts::default(),
                    stale_before_read: DowsePrefetchReadCounts::default(),
                    errors: DowsePrefetchReadCounts::default(),
                },
                replay,
            });
        }

        let execution_started = Arc::new(AtomicBool::new(false));
        let execution_finished = Arc::new(AtomicBool::new(false));
        let actual_head_start_us = Arc::new(AtomicU64::new(0));
        let ready_barrier = Arc::new(Barrier::new(workers + 1));
        let start_barrier = Arc::new(Barrier::new(workers + 1));
        let prefetch_start = Arc::new(OnceLock::new());
        let parent_hash = block.header().parent_hash;

        let (replay, prefetch) = thread::scope(|scope| -> EyreResult<_> {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                let provider = provider.clone();
                let cache = cache.clone();
                let scheduler = scheduler.clone();
                let execution_started = Arc::clone(&execution_started);
                let execution_finished = Arc::clone(&execution_finished);
                let ready_barrier = Arc::clone(&ready_barrier);
                let start_barrier = Arc::clone(&start_barrier);
                handles.push(scope.spawn(move || -> EyreResult<_> {
                    let mut before = DowsePrefetchReadCounts::default();
                    let mut during = DowsePrefetchReadCounts::default();
                    let mut after = DowsePrefetchReadCounts::default();
                    let mut cache_hits = DowsePrefetchReadCounts::default();
                    let mut stale = DowsePrefetchReadCounts::default();
                    let mut errors = DowsePrefetchReadCounts::default();
                    ready_barrier.wait();
                    start_barrier.wait();
                    if execution_finished.load(Ordering::Acquire) {
                        return Ok((before, during, after, cache_hits, stale, errors));
                    }
                    let state_provider = provider.state_by_block_hash(parent_hash)?;

                    loop {
                        let work = scheduler.try_next_work_batch(replay_config.locality_batch_size);
                        if work.is_empty() {
                            if execution_finished.load(Ordering::Acquire) {
                                break;
                            }
                            thread::yield_now();
                            continue;
                        }

                        for work in work {
                            if scheduler.cancel_work_if_stale(&work) {
                                match work.target {
                                    DowsePrefetchTarget::Account(_) => stale.accounts += 1,
                                    DowsePrefetchTarget::Storage { .. } => stale.storage += 1,
                                }
                                continue;
                            }
                            if execution_finished.load(Ordering::Acquire) {
                                scheduler.complete_work(&work);
                                continue;
                            }

                            match work.target {
                                DowsePrefetchTarget::Account(address) => {
                                    let account = match cache
                                        .get_or_try_insert_account_with(address, || {
                                            state_provider.basic_account(&address)
                                        }) {
                                        Ok(CachedStatus::Cached(account)) => {
                                            cache_hits.accounts += 1;
                                            account
                                        }
                                        Ok(CachedStatus::NotCached(account)) => {
                                            let completed = if execution_finished
                                                .load(Ordering::Acquire)
                                            {
                                                &mut after
                                            } else if execution_started.load(Ordering::Acquire) {
                                                &mut during
                                            } else {
                                                &mut before
                                            };
                                            completed.accounts += 1;
                                            account
                                        }
                                        Err(_) => {
                                            errors.accounts += 1;
                                            scheduler.complete_work(&work);
                                            continue;
                                        }
                                    };

                                    if let Some(code_hash) =
                                        account.and_then(|info| info.bytecode_hash)
                                        && scheduler.claim_code_hash(parent_hash, code_hash)
                                    {
                                        match cache.get_or_try_insert_code_with(code_hash, || {
                                            state_provider.bytecode_by_hash(&code_hash)
                                        }) {
                                            Ok(CachedStatus::Cached(_)) => cache_hits.bytecode += 1,
                                            Ok(CachedStatus::NotCached(_)) => {
                                                let completed = if execution_finished
                                                    .load(Ordering::Acquire)
                                                {
                                                    &mut after
                                                } else if execution_started.load(Ordering::Acquire)
                                                {
                                                    &mut during
                                                } else {
                                                    &mut before
                                                };
                                                completed.bytecode += 1;
                                            }
                                            Err(_) => errors.bytecode += 1,
                                        }
                                    }
                                }
                                DowsePrefetchTarget::Storage { address, slot } => {
                                    match cache.get_or_try_insert_storage_with(
                                        address,
                                        slot,
                                        || {
                                            state_provider
                                                .storage(address, slot)
                                                .map(Option::unwrap_or_default)
                                        },
                                    ) {
                                        Ok(CachedStatus::Cached(_)) => cache_hits.storage += 1,
                                        Ok(CachedStatus::NotCached(_)) => {
                                            let completed = if execution_finished
                                                .load(Ordering::Acquire)
                                            {
                                                &mut after
                                            } else if execution_started.load(Ordering::Acquire) {
                                                &mut during
                                            } else {
                                                &mut before
                                            };
                                            completed.storage += 1;
                                        }
                                        Err(_) => errors.storage += 1,
                                    }
                                }
                            }
                            scheduler.complete_work(&work);
                        }
                    }

                    Ok((before, during, after, cache_hits, stale, errors))
                }));
            }

            ready_barrier.wait();
            let actual_head_start = Arc::clone(&actual_head_start_us);
            let started = Arc::clone(&execution_started);
            let worker_start = Arc::clone(&prefetch_start);
            let worker_start_barrier = Arc::clone(&start_barrier);
            let replay = meter_block_with_cache_callbacks(
                provider,
                chain_spec,
                block,
                Some(cache),
                || {
                    let start = *worker_start.get_or_init(Instant::now);
                    worker_start_barrier.wait();
                    thread::sleep(Duration::from_micros(replay_config.head_start_us));
                    let elapsed = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                    actual_head_start.store(elapsed, Ordering::Release);
                    started.store(true, Ordering::Release);
                },
                |tx_hash| scheduler.start_transaction(parent_hash, tx_hash),
                |tx_hash| scheduler.finish_transaction(parent_hash, tx_hash),
            );
            execution_finished.store(true, Ordering::Release);
            if prefetch_start.get().is_none() {
                prefetch_start.get_or_init(Instant::now);
                start_barrier.wait();
            }

            let mut completed_before_execution = DowsePrefetchReadCounts::default();
            let mut completed_during_execution = DowsePrefetchReadCounts::default();
            let mut completed_after_execution = DowsePrefetchReadCounts::default();
            let mut cache_hits = DowsePrefetchReadCounts::default();
            let mut stale_before_read = DowsePrefetchReadCounts::default();
            let mut read_errors = DowsePrefetchReadCounts::default();
            for handle in handles {
                let (before, during, after, cached, stale, errors) =
                    handle.join().map_err(|_| eyre!("Dowse replay worker panicked"))??;
                completed_before_execution.accounts += before.accounts;
                completed_before_execution.storage += before.storage;
                completed_before_execution.bytecode += before.bytecode;
                completed_during_execution.accounts += during.accounts;
                completed_during_execution.storage += during.storage;
                completed_during_execution.bytecode += during.bytecode;
                completed_after_execution.accounts += after.accounts;
                completed_after_execution.storage += after.storage;
                completed_after_execution.bytecode += after.bytecode;
                cache_hits.accounts += cached.accounts;
                cache_hits.storage += cached.storage;
                cache_hits.bytecode += cached.bytecode;
                stale_before_read.accounts += stale.accounts;
                stale_before_read.storage += stale.storage;
                stale_before_read.bytecode += stale.bytecode;
                read_errors.accounts += errors.accounts;
                read_errors.storage += errors.storage;
                read_errors.bytecode += errors.bytecode;
            }

            Ok((
                replay?,
                DowseConcurrentPrefetchStats {
                    planning_time_us,
                    prefetch_time_us: prefetch_start
                        .get()
                        .expect("Dowse workers must have a start time")
                        .elapsed()
                        .as_micros(),
                    actual_head_start_us: u128::from(actual_head_start_us.load(Ordering::Acquire)),
                    planned_transactions,
                    account_targets,
                    storage_targets,
                    workers,
                    completed_before_execution,
                    completed_during_execution,
                    completed_after_execution,
                    cache_hits,
                    stale_before_read,
                    errors: read_errors,
                },
            ))
        })?;

        Ok(DowseConcurrentBlockReplayResponse { config: replay_config, prefetch, replay })
    }
}

/// Plans and prefetches canonical transactions, then replays the block with and without the cache.
pub fn benchmark_dowse_block<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
    cached_first: bool,
) -> EyreResult<DowseBlockBenchmarkResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + Clone + Sync,
{
    let raw = (!cached_first)
        .then(|| meter_block(provider.clone(), Arc::clone(&chain_spec), block))
        .transpose()?;
    let (cache, prefetch) = prefetch_dowse_block(provider.clone(), block, config)?;
    let cached = meter_block_with_optional_cache(
        provider.clone(),
        Arc::clone(&chain_spec),
        block,
        Some(cache),
        || {},
    )?;
    let raw = match raw {
        Some(raw) => raw,
        None => meter_block(provider, chain_spec, block)?,
    };

    eyre::ensure!(
        raw.transactions.len() == cached.transactions.len()
            && raw
                .transactions
                .iter()
                .zip(&cached.transactions)
                .all(|(raw, cached)| raw.tx_hash == cached.tx_hash
                    && raw.gas_used == cached.gas_used),
        "raw and cache-backed Dowse replays produced different transaction results"
    );

    Ok(DowseBlockBenchmarkResponse { cached_first, prefetch, raw, cached })
}

/// Replays one canonical block exactly once, optionally using a cache populated from Dowse plans.
pub fn replay_dowse_block<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
    dowse_cache_enabled: bool,
) -> EyreResult<DowseBlockReplayResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + Clone + Sync,
{
    let (cache, prefetch) = if dowse_cache_enabled {
        let (cache, prefetch) = prefetch_dowse_block(provider.clone(), block, config)?;
        (Some(cache), Some(prefetch))
    } else {
        (None, None)
    };
    let replay = meter_block_with_optional_cache(provider, chain_spec, block, cache, || {})?;

    Ok(DowseBlockReplayResponse { dowse_cache_enabled, prefetch, replay })
}

fn prefetch_dowse_block<P>(
    provider: P,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
) -> EyreResult<(ExecutionCache, DowsePrefetchStats)>
where
    P: StateProviderFactory + Clone + Sync,
{
    let planning_start = Instant::now();
    let planner = PrefetchPlanner::new(&config.hints, PlanLimits::new(32, 256));
    let mut accounts = HashSet::new();
    let mut storage = HashSet::new();
    let mut planned_transactions = 0;

    for transaction in block.body().transactions() {
        let Some(target) = transaction.to() else {
            continue;
        };
        let sender = transaction
            .recover_signer()
            .map_err(|error| eyre!("failed to recover transaction signer: {error}"))?;
        let Some(plan) = planner.plan(target, sender, transaction.input()) else {
            continue;
        };
        planned_transactions += 1;
        accounts.extend(plan.accounts);
        storage.extend(plan.storage);
    }

    let planning_time_us = planning_start.elapsed().as_micros();
    let account_targets = accounts.len();
    let storage_targets = storage.len();
    let mut accounts: Vec<_> = accounts.into_iter().collect();
    accounts.sort_unstable();
    let mut storage: Vec<_> = storage.into_iter().collect();
    storage.sort_unstable_by_key(|target| (target.address, target.slot));
    let cache = ExecutionCache::new(config.cache_size_bytes);

    let prefetch_start = Instant::now();
    let total_targets = accounts.len() + storage.len();
    let workers = config.worker_count.min(total_targets);
    let bytecode_targets = if total_targets == 0 {
        0
    } else {
        thread::scope(|scope| -> EyreResult<usize> {
            let mut handles = Vec::with_capacity(workers);
            let accounts = &accounts;
            let storage = &storage;
            for worker in 0..workers {
                let provider = provider.clone();
                let cache = cache.clone();
                let parent_hash = block.header().parent_hash;
                handles.push(scope.spawn(move || -> EyreResult<usize> {
                    let state_provider = provider.state_by_block_hash(parent_hash)?;
                    let mut bytecode_targets = 0;

                    for address in accounts.iter().skip(worker).step_by(workers) {
                        let account = state_provider.basic_account(address)?;
                        cache.insert_account(*address, account);
                        if let Some(code_hash) = account.and_then(|info| info.bytecode_hash) {
                            let code = state_provider.bytecode_by_hash(&code_hash)?;
                            cache.insert_code(code_hash, code);
                            bytecode_targets += 1;
                        }
                    }
                    let storage_offset = (worker + workers - accounts.len() % workers) % workers;
                    for target in storage.iter().skip(storage_offset).step_by(workers) {
                        let value = state_provider.storage(target.address, target.slot)?;
                        cache.insert_storage(target.address, target.slot, value);
                    }

                    Ok(bytecode_targets)
                }));
            }

            handles.into_iter().try_fold(0, |total, handle| {
                let count =
                    handle.join().map_err(|_| eyre!("Dowse benchmark worker panicked"))??;
                Ok(total + count)
            })
        })?
    };
    let prefetch_time_us = prefetch_start.elapsed().as_micros();

    Ok((
        cache,
        DowsePrefetchStats {
            planning_time_us,
            prefetch_time_us,
            planned_transactions,
            account_targets,
            storage_targets,
            bytecode_targets,
            workers,
        },
    ))
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockHeader, Header};
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolCall;
    use base_common_consensus::{BaseBlockBody, BaseTransactionSigned};
    use base_node_runner::test_utils::TestHarness;
    use base_test_utils::{Account, SimpleStorage};
    use dowse_types::{PrefetchItem, SlotExpression};
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    #[tokio::test]
    async fn replays_identical_block_with_prefetched_storage() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let (deployment, target, _) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment]).await?;
        let signed = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(target)
            .gas_limit(100_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();
        let transaction = BaseTransactionSigned::Eip1559(
            signed.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let latest = harness.latest_block();
        let block = BaseBlock::new(
            Header {
                parent_hash: latest.hash(),
                number: latest.number() + 1,
                timestamp: latest.timestamp() + 2,
                gas_limit: 30_000_000,
                beneficiary: Address::random(),
                base_fee_per_gas: Some(1),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            BaseBlockBody { transactions: vec![transaction], ommers: vec![], withdrawals: None },
        );
        let mut hints = HintTable::new();
        hints.insert(
            target,
            B256::repeat_byte(0x11),
            None,
            vec![PrefetchItem::Storage { slot: SlotExpression::Concrete { value: B256::ZERO } }],
        );
        let config = DowseBenchmarkConfig {
            hints: Arc::new(hints),
            worker_count: 2,
            cache_size_bytes: 1_000_000,
        };

        for cached_first in [false, true] {
            let response = benchmark_dowse_block(
                harness.blockchain_provider(),
                harness.chain_spec(),
                &block,
                &config,
                cached_first,
            )?;

            assert_eq!(response.cached_first, cached_first);
            assert_eq!(response.prefetch.planned_transactions, 1);
            assert_eq!(response.prefetch.storage_targets, 1);
            assert_eq!(
                response.raw.transactions[0].gas_used,
                response.cached.transactions[0].gas_used
            );
            assert!(response.raw.state_provider.storage_fetches > 0);
            assert_eq!(response.cached.state_provider.storage_fetches, 0);
        }

        let raw = replay_dowse_block(
            harness.blockchain_provider(),
            harness.chain_spec(),
            &block,
            &config,
            false,
        )?;
        let cached = replay_dowse_block(
            harness.blockchain_provider(),
            harness.chain_spec(),
            &block,
            &config,
            true,
        )?;
        assert!(!raw.dowse_cache_enabled);
        assert!(raw.prefetch.is_none());
        assert!(cached.dowse_cache_enabled);
        assert_eq!(cached.prefetch.as_ref().unwrap().planned_transactions, 1);
        assert_eq!(raw.replay.transactions[0].gas_used, cached.replay.transactions[0].gas_used);
        assert!(raw.replay.state_provider.storage_fetches > 0);
        assert_eq!(cached.replay.state_provider.storage_fetches, 0);

        let concurrent = config.replay_concurrent_block(
            harness.blockchain_provider(),
            harness.chain_spec(),
            &block,
            DowseConcurrentReplayConfig {
                workers: 1,
                head_start_us: 50_000,
                max_accounts_per_transaction: 32,
                max_storage_slots_per_transaction: 256,
                max_transaction_distance: 2_048,
                locality_batch_size: 1,
                min_confidence_bps: 0,
            },
        )?;
        assert_eq!(raw.replay.transactions[0].gas_used, concurrent.replay.transactions[0].gas_used);
        assert_eq!(concurrent.prefetch.planned_transactions, 1);
        assert_eq!(concurrent.prefetch.storage_targets, 1);
        assert_eq!(concurrent.prefetch.completed_before_execution.storage, 1);
        assert!(concurrent.prefetch.actual_head_start_us >= 50_000);
        assert_eq!(concurrent.replay.state_provider.storage_fetches, 0);
        Ok(())
    }
}
