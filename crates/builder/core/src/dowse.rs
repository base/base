//! Background state prefetching from bounded Dowse transaction plans.

use std::{
    collections::HashSet,
    fmt, fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction;
use alloy_primitives::B256;
use dowse_plan::{PlanLimits, PrefetchPlan, PrefetchPlanner};
use dowse_types::HintTable;
use parking_lot::RwLock;
use reth_execution_cache::ExecutionCache;
use reth_provider::{StateProviderBox, StateProviderFactory};
use reth_tasks::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{BuilderMetrics, PoolBounds};

/// Configuration for hint-driven transaction-pool state prefetching.
#[derive(Clone)]
pub struct DowseConfig {
    /// Validated hint table used to plan state reads.
    pub hints: Arc<HintTable>,
    /// Alternate cache use by parent hash while keeping background prefetching enabled.
    pub ab_test: bool,
    /// Number of persistent blocking workers performing state reads.
    pub worker_count: usize,
    /// Maximum queued transaction plans per worker.
    pub queue_capacity: usize,
    /// Interval between transaction-pool scans.
    pub poll_interval: Duration,
    /// Maximum best transactions examined on each scan.
    pub max_transactions: usize,
    /// Maximum account targets emitted for one transaction.
    pub max_accounts_per_transaction: usize,
    /// Maximum storage targets emitted for one transaction.
    pub max_storage_slots_per_transaction: usize,
    /// Total byte budget used to size the parent-state execution cache.
    pub cache_size_bytes: usize,
}

impl DowseConfig {
    /// Loads and validates a version 1 JSON hint table.
    pub fn load_hints(path: impl AsRef<Path>) -> eyre::Result<Arc<HintTable>> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|error| {
            eyre::eyre!("failed to read Dowse hints at {}: {error}", path.display())
        })?;
        let hints: HintTable = serde_json::from_slice(&bytes).map_err(|error| {
            eyre::eyre!("failed to parse Dowse hints at {}: {error}", path.display())
        })?;
        eyre::ensure!(hints.version == 1, "unsupported Dowse hint table version {}", hints.version);
        Ok(Arc::new(hints))
    }

    /// Returns whether a payload on `parent_hash` should consult the Dowse cache.
    pub fn cache_enabled_for(&self, parent_hash: B256) -> bool {
        !self.ab_test || parent_hash.as_slice()[0].is_multiple_of(2)
    }
}

impl fmt::Debug for DowseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DowseConfig")
            .field("hint_selectors", &self.hints.selector_count())
            .field("hint_items", &self.hints.item_count())
            .field("ab_test", &self.ab_test)
            .field("worker_count", &self.worker_count)
            .field("queue_capacity", &self.queue_capacity)
            .field("poll_interval", &self.poll_interval)
            .field("max_transactions", &self.max_transactions)
            .field("max_accounts_per_transaction", &self.max_accounts_per_transaction)
            .field("max_storage_slots_per_transaction", &self.max_storage_slots_per_transaction)
            .field("cache_size_bytes", &self.cache_size_bytes)
            .finish()
    }
}

/// Parent-hash-tagged cache shared by Dowse workers and the payload builder.
#[derive(Clone, Debug, Default)]
pub struct DowsePrefetchCache {
    inner: Arc<RwLock<Option<(B256, ExecutionCache)>>>,
}

impl DowsePrefetchCache {
    /// Replaces the active cache with an empty cache for `parent_hash`.
    pub fn activate(&self, parent_hash: B256, cache_size_bytes: usize) -> ExecutionCache {
        let cache = ExecutionCache::new(cache_size_bytes);
        *self.inner.write() = Some((parent_hash, cache.clone()));
        cache
    }

    /// Returns the active cache only when it was populated from `parent_hash` state.
    pub fn cache_for(&self, parent_hash: B256) -> Option<ExecutionCache> {
        self.inner
            .read()
            .as_ref()
            .filter(|(cached_parent, _)| *cached_parent == parent_hash)
            .map(|(_, cache)| cache.clone())
    }
}

/// One bounded prefetch plan sent to a background worker.
#[derive(Debug)]
pub struct DowsePrefetchWork {
    /// State root context against which all reads must execute.
    pub parent_hash: B256,
    /// Cache receiving values read by this work item.
    pub cache: ExecutionCache,
    /// Concrete state targets resolved from transaction context.
    pub plan: PrefetchPlan,
    /// Cancellation signal fired when the parent changes.
    pub cancel: CancellationToken,
}

/// Coordinates txpool planning and persistent background state-read workers.
#[derive(Debug)]
pub struct DowsePrefetcher<Client, Pool> {
    client: Client,
    pool: Pool,
    config: DowseConfig,
    cache: DowsePrefetchCache,
}

impl<Client, Pool> DowsePrefetcher<Client, Pool>
where
    Client: StateProviderFactory + Clone + Send + Sync + 'static,
    Pool: PoolBounds,
{
    /// Creates a prefetcher and its parent-tagged cache handle.
    pub fn new(client: Client, pool: Pool, config: DowseConfig) -> Self {
        Self { client, pool, config, cache: DowsePrefetchCache::default() }
    }

    /// Spawns persistent workers and the transaction-pool planning loop.
    ///
    /// Returns the cache handle that payload builders should consult for their exact parent hash.
    pub fn spawn(self, executor: Runtime) -> DowsePrefetchCache {
        let shared_cache = self.cache.clone();
        let worker_count = self.config.worker_count;
        let poll_interval = self.config.poll_interval;
        let mut worker_senders = Vec::with_capacity(self.config.worker_count);

        for _ in 0..self.config.worker_count {
            let (sender, mut receiver) =
                mpsc::channel::<DowsePrefetchWork>(self.config.queue_capacity);
            worker_senders.push(sender);
            let client = self.client.clone();

            executor.spawn_blocking_task(async move {
                let mut provider: Option<(B256, StateProviderBox)> = None;

                while let Some(work) = receiver.recv().await {
                    if work.cancel.is_cancelled() {
                        continue;
                    }

                    let start = Instant::now();
                    if provider
                        .as_ref()
                        .is_none_or(|(parent_hash, _)| *parent_hash != work.parent_hash)
                    {
                        match client.state_by_block_hash(work.parent_hash) {
                            Ok(state_provider) => {
                                provider = Some((work.parent_hash, state_provider))
                            }
                            Err(error) => {
                                BuilderMetrics::dowse_prefetch_reads_total("provider", "error")
                                    .increment(1);
                                warn!(
                                    parent_hash = %work.parent_hash,
                                    error = %error,
                                    "failed to open parent state for Dowse prefetching"
                                );
                                continue;
                            }
                        }
                    }
                    let state_provider =
                        &provider.as_ref().expect("provider initialized for current Dowse work").1;

                    for address in work.plan.accounts {
                        if work.cancel.is_cancelled() {
                            break;
                        }

                        match state_provider.basic_account(&address) {
                            Ok(account) => {
                                BuilderMetrics::dowse_prefetch_reads_total("account", "success")
                                    .increment(1);
                                work.cache.insert_account(address, account);

                                if let Some(code_hash) = account.and_then(|info| info.bytecode_hash)
                                {
                                    match state_provider.bytecode_by_hash(&code_hash) {
                                        Ok(code) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "bytecode", "success",
                                            )
                                            .increment(1);
                                            work.cache.insert_code(code_hash, code);
                                        }
                                        Err(error) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "bytecode", "error",
                                            )
                                            .increment(1);
                                            warn!(
                                                %address,
                                                %code_hash,
                                                error = %error,
                                                "Dowse bytecode prefetch failed"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                BuilderMetrics::dowse_prefetch_reads_total("account", "error")
                                    .increment(1);
                                warn!(%address, error = %error, "Dowse account prefetch failed");
                            }
                        }
                    }

                    for target in work.plan.storage {
                        if work.cancel.is_cancelled() {
                            break;
                        }

                        match state_provider.storage(target.address, target.slot) {
                            Ok(value) => {
                                BuilderMetrics::dowse_prefetch_reads_total("storage", "success")
                                    .increment(1);
                                work.cache.insert_storage(target.address, target.slot, value);
                            }
                            Err(error) => {
                                BuilderMetrics::dowse_prefetch_reads_total("storage", "error")
                                    .increment(1);
                                warn!(
                                    address = %target.address,
                                    slot = %target.slot,
                                    error = %error,
                                    "Dowse storage prefetch failed"
                                );
                            }
                        }
                    }

                    BuilderMetrics::dowse_prefetch_work_duration().record(start.elapsed());
                }
            });
        }

        let pool = self.pool;
        let config = self.config;
        let cache = self.cache;
        executor.spawn_task(async move {
            let mut timer = tokio::time::interval(config.poll_interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let planner = PrefetchPlanner::new(
                &config.hints,
                PlanLimits::new(
                    config.max_accounts_per_transaction,
                    config.max_storage_slots_per_transaction,
                ),
            );
            let mut active_parent = None;
            let mut parent_cancel = CancellationToken::new();
            let mut seen_transactions = HashSet::new();
            let mut seen_accounts = HashSet::new();
            let mut seen_storage = HashSet::new();
            let mut next_worker = 0;

            loop {
                timer.tick().await;
                let parent_hash = pool.block_info().last_seen_block_hash;
                let active_cache = if active_parent == Some(parent_hash) {
                    cache.cache_for(parent_hash).expect("active Dowse parent must have a cache")
                } else {
                    parent_cancel.cancel();
                    parent_cancel = CancellationToken::new();
                    seen_transactions.clear();
                    seen_accounts.clear();
                    seen_storage.clear();
                    active_parent = Some(parent_hash);
                    BuilderMetrics::dowse_parent_resets_total().increment(1);
                    cache.activate(parent_hash, config.cache_size_bytes)
                };

                let mut best_transactions = pool.best_transactions();
                for transaction in best_transactions.by_ref().take(config.max_transactions) {
                    let tx_hash = *transaction.hash();
                    if !seen_transactions.insert(tx_hash) {
                        continue;
                    }

                    let Some(target) = transaction.to() else {
                        BuilderMetrics::dowse_transactions_total("contract_creation").increment(1);
                        continue;
                    };
                    let Some(mut plan) =
                        planner.plan(target, transaction.sender(), transaction.transaction.input())
                    else {
                        BuilderMetrics::dowse_transactions_total("no_hints").increment(1);
                        continue;
                    };

                    let original_target_count = plan.target_count();
                    plan.accounts.retain(|address| !seen_accounts.contains(address));
                    plan.storage.retain(|target| !seen_storage.contains(target));
                    BuilderMetrics::dowse_plan_items_omitted_total("parent_duplicate")
                        .increment((original_target_count - plan.target_count()) as u64);

                    BuilderMetrics::dowse_plan_targets("account")
                        .record(plan.accounts.len() as f64);
                    BuilderMetrics::dowse_plan_targets("storage").record(plan.storage.len() as f64);
                    BuilderMetrics::dowse_plan_items_omitted_total("unresolved")
                        .increment(plan.diagnostics.unresolved_items as u64);
                    BuilderMetrics::dowse_plan_items_omitted_total("truncated")
                        .increment(plan.diagnostics.truncated_items as u64);
                    BuilderMetrics::dowse_plan_items_omitted_total("duplicate")
                        .increment(plan.diagnostics.duplicate_items as u64);

                    if plan.target_count() == 0 {
                        BuilderMetrics::dowse_transactions_total("empty_plan").increment(1);
                        continue;
                    }

                    seen_accounts.extend(plan.accounts.iter().copied());
                    seen_storage.extend(plan.storage.iter().copied());
                    let work = DowsePrefetchWork {
                        parent_hash,
                        cache: active_cache.clone(),
                        plan,
                        cancel: parent_cancel.clone(),
                    };
                    let mut pending_work = Some(work);
                    for offset in 0..worker_senders.len() {
                        let worker = (next_worker + offset) % worker_senders.len();
                        match worker_senders[worker]
                            .try_send(pending_work.take().expect("work not yet queued"))
                        {
                            Ok(()) => {
                                next_worker = (worker + 1) % worker_senders.len();
                                BuilderMetrics::dowse_transactions_total("planned").increment(1);
                                break;
                            }
                            Err(
                                mpsc::error::TrySendError::Full(work)
                                | mpsc::error::TrySendError::Closed(work),
                            ) => {
                                pending_work = Some(work);
                            }
                        }
                    }

                    if let Some(work) = pending_work {
                        for address in &work.plan.accounts {
                            seen_accounts.remove(address);
                        }
                        for target in &work.plan.storage {
                            seen_storage.remove(target);
                        }
                        seen_transactions.remove(&tx_hash);
                        BuilderMetrics::dowse_queue_drops_total().increment(1);
                        BuilderMetrics::dowse_transactions_total("queue_full").increment(1);
                    }
                }
            }
        });

        info!(
            workers = worker_count,
            poll_interval_ms = poll_interval.as_millis(),
            "Dowse transaction-pool prefetcher started"
        );
        shared_cache
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use reth_execution_cache::CachedStatus;

    use super::*;

    #[test]
    fn cache_is_only_returned_for_exact_parent() {
        let parent = B256::random();
        let other = B256::random();
        let cache = DowsePrefetchCache::default();

        assert!(cache.cache_for(parent).is_none());
        let activated = cache.activate(parent, 1_000_000);
        let address = Address::random();
        let slot = B256::random();
        let value = U256::from(7);
        activated.insert_storage(address, slot, Some(value));

        assert!(cache.cache_for(other).is_none());
        assert_eq!(
            cache
                .cache_for(parent)
                .unwrap()
                .get_or_try_insert_storage_with(address, slot, || Ok::<_, ()>(U256::ZERO))
                .unwrap(),
            CachedStatus::Cached(value),
            "exact-parent lookup should return the activated cache"
        );

        cache.activate(other, 1_000_000);
        assert!(cache.cache_for(parent).is_none(), "superseded parent must become inaccessible");
    }

    #[test]
    fn ab_test_alternates_cache_use_by_parent_hash() {
        let mut config = DowseConfig {
            hints: Arc::new(HintTable::default()),
            ab_test: false,
            worker_count: 1,
            queue_capacity: 1,
            poll_interval: Duration::from_millis(1),
            max_transactions: 1,
            max_accounts_per_transaction: 1,
            max_storage_slots_per_transaction: 1,
            cache_size_bytes: 1,
        };
        let even_parent = B256::repeat_byte(2);
        let odd_parent = B256::repeat_byte(1);

        assert!(config.cache_enabled_for(even_parent));
        assert!(config.cache_enabled_for(odd_parent));

        config.ab_test = true;
        assert!(config.cache_enabled_for(even_parent));
        assert!(!config.cache_enabled_for(odd_parent));
    }
}
