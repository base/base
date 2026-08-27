//! Background state prefetching from bounded Dowse transaction plans.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    fmt, fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256, TxHash, keccak256};
use dowse_plan::{PlanLimits, PrefetchPlan, PrefetchPlanner, StorageTarget};
use dowse_types::HintTable;
use parking_lot::{Mutex, RwLock};
use reth_execution_cache::{CachedStatus, ExecutionCache};
use reth_provider::{StateProviderBox, StateProviderFactory};
use reth_tasks::Runtime;
use reth_transaction_pool::TransactionListenerKind;
use tokio::sync::Notify;
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
    /// Maximum unique state targets waiting in the central priority queue.
    pub queue_capacity: usize,
    /// Maximum txpool positions workers may run ahead of the payload builder cursor.
    pub max_transaction_distance: usize,
    /// Maximum neighboring priority targets a worker may reorder by physical database key.
    pub locality_batch_size: usize,
    /// Minimum hint confidence admitted to the state-read queue, in basis points.
    pub min_confidence_bps: u16,
    /// Maximum interval between transaction-pool priority refreshes.
    pub poll_interval: Duration,
    /// Maximum best transactions examined on each event-driven or periodic refresh.
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
            .field("max_transaction_distance", &self.max_transaction_distance)
            .field("locality_batch_size", &self.locality_batch_size)
            .field("min_confidence_bps", &self.min_confidence_bps)
            .field("poll_interval", &self.poll_interval)
            .field("max_transactions", &self.max_transactions)
            .field("max_accounts_per_transaction", &self.max_accounts_per_transaction)
            .field("max_storage_slots_per_transaction", &self.max_storage_slots_per_transaction)
            .field("cache_size_bytes", &self.cache_size_bytes)
            .finish()
    }
}

/// One concrete state read managed by the central Dowse scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DowsePrefetchTarget {
    /// Basic account information and, when present, the account's bytecode.
    Account(Address),
    /// One account storage slot.
    Storage {
        /// Account whose storage should be read.
        address: Address,
        /// Concrete storage key.
        slot: B256,
    },
}

impl DowsePrefetchTarget {
    /// Returns the target kind used in metrics.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Account(_) => "account",
            Self::Storage { .. } => "storage",
        }
    }

    /// Gives account reads precedence for the same transaction rank because account and bytecode
    /// reads occur before contract storage access during EVM execution.
    pub const fn kind_priority(self) -> u8 {
        match self {
            Self::Account(_) => 1,
            Self::Storage { .. } => 0,
        }
    }

    /// Returns the MDBX table and hashed keys used to improve locality inside a bounded batch.
    pub fn physical_order(self) -> (u8, B256, B256) {
        match self {
            Self::Account(address) => (0, keccak256(address), B256::ZERO),
            Self::Storage { address, slot } => (1, keccak256(address), keccak256(slot)),
        }
    }
}

/// Lifecycle of a target tracked by the central Dowse scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowsePrefetchTargetState {
    /// Waiting for a state-read worker.
    Queued,
    /// Claimed by exactly one state-read worker.
    InFlight,
    /// Read or found in the shared execution cache.
    Complete,
    /// No remaining transaction can use this target.
    Stale,
}

/// Scheduler metadata retained for one unique target.
#[derive(Debug)]
pub struct DowsePrefetchTargetRecord {
    state: DowsePrefetchTargetState,
    version: u64,
    transaction_priorities: HashMap<TxHash, (usize, u16)>,
    queued_at: Instant,
}

impl DowsePrefetchTargetRecord {
    /// Returns the nearest transaction rank and its strongest confidence for this target.
    pub fn best_priority(&self) -> Option<(usize, u16)> {
        self.transaction_priorities.values().copied().min_by(
            |(left_rank, left_confidence), (right_rank, right_confidence)| {
                left_rank.cmp(right_rank).then_with(|| right_confidence.cmp(left_confidence))
            },
        )
    }
}

/// Mutable state protected by the central scheduler lock.
#[derive(Debug)]
pub struct DowsePrefetchSchedulerState {
    active_parent: Option<B256>,
    active_cache: Option<ExecutionCache>,
    queue: BinaryHeap<(bool, Reverse<usize>, u16, u8, Reverse<u64>, DowsePrefetchTarget, u64)>,
    urgent_queue: VecDeque<(DowsePrefetchTarget, u64)>,
    targets: HashMap<DowsePrefetchTarget, DowsePrefetchTargetRecord>,
    transaction_targets: HashMap<TxHash, Vec<DowsePrefetchTarget>>,
    transaction_ranks: HashMap<TxHash, usize>,
    claimed_code_hashes: HashSet<B256>,
    current_transaction: Option<TxHash>,
    builder_rank: Option<usize>,
    next_sequence: u64,
    queued_targets: usize,
    in_flight_targets: usize,
}

impl Default for DowsePrefetchSchedulerState {
    fn default() -> Self {
        Self {
            active_parent: None,
            active_cache: None,
            queue: BinaryHeap::new(),
            urgent_queue: VecDeque::new(),
            targets: HashMap::new(),
            transaction_targets: HashMap::new(),
            transaction_ranks: HashMap::new(),
            claimed_code_hashes: HashSet::new(),
            current_transaction: None,
            builder_rank: None,
            next_sequence: 0,
            queued_targets: 0,
            in_flight_targets: 0,
        }
    }
}

/// Parent-hash-tagged cache shared by Dowse workers and the payload builder.
#[derive(Clone, Debug)]
pub struct DowsePrefetchCache {
    inner: Arc<RwLock<Option<(B256, ExecutionCache)>>>,
    scheduler: Arc<Mutex<DowsePrefetchSchedulerState>>,
    notify: Arc<Notify>,
    queue_capacity: usize,
    max_transaction_distance: usize,
    min_confidence_bps: u16,
}

impl DowsePrefetchCache {
    /// Creates a parent cache and scheduler with a bounded target queue.
    pub fn new(
        queue_capacity: usize,
        max_transaction_distance: usize,
        min_confidence_bps: u16,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            scheduler: Arc::new(Mutex::new(DowsePrefetchSchedulerState::default())),
            notify: Arc::new(Notify::new()),
            queue_capacity,
            max_transaction_distance,
            min_confidence_bps,
        }
    }

    /// Converts a normalized confidence score into scheduler basis points.
    pub fn confidence_bps(confidence: f64) -> u16 {
        if confidence.is_finite() {
            (confidence.clamp(0.0, 1.0) * 10_000.0).round() as u16
        } else {
            0
        }
    }

    /// Replaces the active cache with an empty cache for `parent_hash`.
    pub fn activate(&self, parent_hash: B256, cache_size_bytes: usize) -> ExecutionCache {
        let cache = ExecutionCache::new(cache_size_bytes);
        let mut scheduler = self.scheduler.lock();
        *self.inner.write() = Some((parent_hash, cache.clone()));
        *scheduler = DowsePrefetchSchedulerState {
            active_parent: Some(parent_hash),
            active_cache: Some(cache.clone()),
            ..Default::default()
        };
        BuilderMetrics::dowse_queue_depth().set(0.0);
        BuilderMetrics::dowse_prefetch_reads_in_flight().set(0.0);
        self.notify.notify_waiters();
        cache
    }

    /// Returns the exact-parent cache, creating it if the planner has not observed the parent yet.
    pub fn cache_for_or_activate(
        &self,
        parent_hash: B256,
        cache_size_bytes: usize,
    ) -> ExecutionCache {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent == Some(parent_hash) {
            return scheduler.active_cache.clone().expect("active parent must have a cache");
        }

        let cache = ExecutionCache::new(cache_size_bytes);
        *self.inner.write() = Some((parent_hash, cache.clone()));
        *scheduler = DowsePrefetchSchedulerState {
            active_parent: Some(parent_hash),
            active_cache: Some(cache.clone()),
            ..Default::default()
        };
        BuilderMetrics::dowse_queue_depth().set(0.0);
        BuilderMetrics::dowse_prefetch_reads_in_flight().set(0.0);
        drop(scheduler);
        self.notify.notify_waiters();
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

    /// Returns whether this transaction already has targets registered for `parent_hash`.
    pub fn contains_transaction(&self, parent_hash: B256, tx_hash: TxHash) -> bool {
        let scheduler = self.scheduler.lock();
        scheduler.active_parent == Some(parent_hash)
            && scheduler.transaction_targets.contains_key(&tx_hash)
    }

    /// Records one txpool transaction's current priority rank, including transactions without hints.
    pub fn observe_transaction_rank(&self, parent_hash: B256, tx_hash: TxHash, rank: usize) {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent == Some(parent_hash) {
            scheduler.transaction_ranks.insert(tx_hash, rank);
        }
    }

    /// Inserts one transaction's target plan and queues every newly discovered target.
    pub fn submit_plan(&self, parent_hash: B256, tx_hash: TxHash, rank: usize, plan: PrefetchPlan) {
        let targets = plan
            .accounts
            .into_iter()
            .zip(plan.account_confidence.into_iter().chain(std::iter::repeat(1.0)))
            .map(|(address, confidence)| {
                (DowsePrefetchTarget::Account(address), Self::confidence_bps(confidence))
            })
            .chain(
                plan.storage
                    .into_iter()
                    .zip(plan.storage_confidence.into_iter().chain(std::iter::repeat(1.0)))
                    .map(|(StorageTarget { address, slot }, confidence)| {
                        (
                            DowsePrefetchTarget::Storage { address, slot },
                            Self::confidence_bps(confidence),
                        )
                    }),
            )
            .collect::<Vec<_>>();
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(parent_hash) {
            return;
        }
        scheduler.transaction_ranks.insert(tx_hash, rank);

        let mut registered = Vec::with_capacity(targets.len());
        for (target, confidence) in targets {
            if confidence < self.min_confidence_bps {
                BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "low_confidence")
                    .increment(1);
                continue;
            }
            let is_current_transaction = scheduler.current_transaction == Some(tx_hash);
            let queue_has_capacity = scheduler.queued_targets < self.queue_capacity;
            if let Some(record) = scheduler.targets.get_mut(&target) {
                if record.state == DowsePrefetchTargetState::Stale {
                    if !queue_has_capacity {
                        BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "queue_full")
                            .increment(1);
                        continue;
                    }
                    record.state = DowsePrefetchTargetState::Queued;
                    record.version = record.version.wrapping_add(1);
                    record.queued_at = Instant::now();
                    record.transaction_priorities.insert(tx_hash, (rank, confidence));
                    scheduler.queued_targets += 1;
                    registered.push(target);
                    BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "queued")
                        .increment(1);
                    Self::push_queue_entry(&mut scheduler, target);
                    continue;
                }
                let previous_priority = record.best_priority();
                record.transaction_priorities.insert(tx_hash, (rank, confidence));
                let should_requeue = record.state == DowsePrefetchTargetState::Queued
                    && (previous_priority != record.best_priority() || is_current_transaction);
                if should_requeue {
                    record.version = record.version.wrapping_add(1);
                }
                registered.push(target);
                BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "duplicate")
                    .increment(1);
                if should_requeue {
                    Self::push_queue_entry(&mut scheduler, target);
                }
                continue;
            }
            if !queue_has_capacity {
                BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "queue_full")
                    .increment(1);
                continue;
            }

            let current = scheduler.current_transaction == Some(tx_hash);
            let version = 1;
            let sequence = scheduler.next_sequence;
            scheduler.next_sequence = scheduler.next_sequence.wrapping_add(1);
            scheduler.queue.push((
                current,
                Reverse(rank),
                confidence,
                target.kind_priority(),
                Reverse(sequence),
                target,
                version,
            ));
            scheduler.targets.insert(
                target,
                DowsePrefetchTargetRecord {
                    state: DowsePrefetchTargetState::Queued,
                    version,
                    transaction_priorities: HashMap::from([(tx_hash, (rank, confidence))]),
                    queued_at: Instant::now(),
                },
            );
            scheduler.queued_targets += 1;
            registered.push(target);
            BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "queued").increment(1);
        }
        scheduler.transaction_targets.insert(tx_hash, registered);
        BuilderMetrics::dowse_queue_depth().set(scheduler.queued_targets as f64);
        drop(scheduler);
        self.notify.notify_waiters();
    }

    /// Updates a transaction's scheduler rank after a fresh txpool priority snapshot.
    pub fn update_transaction_rank(&self, parent_hash: B256, tx_hash: TxHash, rank: usize) {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(parent_hash) {
            return;
        }
        let Some(targets) = scheduler.transaction_targets.get(&tx_hash).cloned() else {
            return;
        };
        for target in targets {
            let should_requeue = {
                let Some(record) = scheduler.targets.get_mut(&target) else { continue };
                if record
                    .transaction_priorities
                    .get(&tx_hash)
                    .is_some_and(|(previous_rank, _)| *previous_rank == rank)
                {
                    continue;
                }
                let previous_priority = record.best_priority();
                let confidence = record
                    .transaction_priorities
                    .get(&tx_hash)
                    .map_or(10_000, |(_, confidence)| *confidence);
                record.transaction_priorities.insert(tx_hash, (rank, confidence));
                let next_priority = record.best_priority();
                let should_requeue = record.state == DowsePrefetchTargetState::Queued
                    && previous_priority != next_priority;
                if should_requeue {
                    record.version = record.version.wrapping_add(1);
                }
                should_requeue
            };
            if should_requeue {
                Self::push_queue_entry(&mut scheduler, target);
            }
        }
        drop(scheduler);
        self.notify.notify_waiters();
    }

    /// Drops queued dependencies for transactions no longer present in the planner snapshot.
    pub fn retain_transactions(&self, parent_hash: B256, retained: &HashSet<TxHash>) {
        let stale = {
            let scheduler = self.scheduler.lock();
            if scheduler.active_parent != Some(parent_hash) {
                return;
            }
            scheduler
                .transaction_targets
                .keys()
                .filter(|tx_hash| {
                    !retained.contains(*tx_hash)
                        && scheduler.current_transaction.as_ref() != Some(*tx_hash)
                })
                .copied()
                .collect::<Vec<_>>()
        };
        for tx_hash in stale {
            self.finish_transaction(parent_hash, tx_hash);
        }
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent == Some(parent_hash) {
            scheduler.transaction_ranks.retain(|tx_hash, _| retained.contains(tx_hash));
        }
    }

    /// Promotes the exact transaction selected by the payload builder ahead of speculative work.
    pub fn start_transaction(&self, parent_hash: B256, tx_hash: TxHash) {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(parent_hash) {
            return;
        }
        scheduler.current_transaction = Some(tx_hash);
        if let Some(rank) = scheduler.transaction_ranks.get(&tx_hash).copied() {
            scheduler.builder_rank = Some(rank);
        }
        let mut targets = scheduler.transaction_targets.get(&tx_hash).cloned().unwrap_or_default();
        BuilderMetrics::dowse_builder_cursor_total(if targets.is_empty() {
            "unplanned"
        } else {
            "matched"
        })
        .increment(1);
        targets.sort_unstable_by_key(|target| {
            Reverse(
                scheduler
                    .targets
                    .get(target)
                    .and_then(DowsePrefetchTargetRecord::best_priority)
                    .map_or((0, target.kind_priority()), |(_, confidence)| {
                        (confidence, target.kind_priority())
                    }),
            )
        });
        for target in targets {
            let version = {
                let Some(record) = scheduler.targets.get(&target) else { continue };
                if record.state != DowsePrefetchTargetState::Queued {
                    None
                } else {
                    Some(record.version)
                }
            };
            if let Some(version) = version {
                scheduler.urgent_queue.push_back((target, version));
            }
        }
        drop(scheduler);
        self.notify.notify_waiters();
    }

    /// Removes a consumed, rejected, or deferred transaction's target dependencies.
    pub fn finish_transaction(&self, parent_hash: B256, tx_hash: TxHash) {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(parent_hash) {
            return;
        }
        if scheduler.current_transaction == Some(tx_hash) {
            scheduler.current_transaction = None;
        }
        scheduler.transaction_ranks.remove(&tx_hash);
        let Some(targets) = scheduler.transaction_targets.remove(&tx_hash) else { return };
        for target in targets {
            let mut became_stale = false;
            let should_requeue = {
                let Some(record) = scheduler.targets.get_mut(&target) else { continue };
                let previous_priority = record.best_priority();
                record.transaction_priorities.remove(&tx_hash);
                let next_priority = record.best_priority();
                if record.transaction_priorities.is_empty()
                    && record.state == DowsePrefetchTargetState::Queued
                {
                    record.state = DowsePrefetchTargetState::Stale;
                    became_stale = true;
                    false
                } else if record.state == DowsePrefetchTargetState::Queued
                    && previous_priority != next_priority
                {
                    record.version = record.version.wrapping_add(1);
                    true
                } else {
                    false
                }
            };
            if became_stale {
                scheduler.queued_targets -= 1;
                BuilderMetrics::dowse_prefetch_targets_total(target.kind(), "stale").increment(1);
            } else if should_requeue {
                Self::push_queue_entry(&mut scheduler, target);
            }
        }
        BuilderMetrics::dowse_queue_depth().set(scheduler.queued_targets as f64);
    }

    /// Waits for and claims the highest-priority target not already owned by another worker.
    pub async fn next_work(&self) -> DowsePrefetchWork {
        loop {
            let notified = self.notify.notified();
            if let Some(work) = self.try_claim_work() {
                return work;
            }
            notified.await;
        }
    }

    /// Waits for work, claims up to `max_targets`, then sorts that bounded batch by MDBX key.
    pub async fn next_work_batch(&self, max_targets: usize) -> Vec<DowsePrefetchWork> {
        let first = self.next_work().await;
        let mut work = Vec::with_capacity(max_targets);
        work.push(first);
        while work.len() < max_targets {
            let Some(next) = self.try_claim_work() else { break };
            work.push(next);
        }
        work.sort_unstable_by_key(|work| work.target.physical_order());
        work
    }

    /// Claims the next target immediately, or returns `None` when the queue is empty.
    pub fn try_next_work(&self) -> Option<DowsePrefetchWork> {
        self.try_claim_work()
    }

    /// Claims and physically orders up to `max_targets` without waiting for new work.
    pub fn try_next_work_batch(&self, max_targets: usize) -> Vec<DowsePrefetchWork> {
        let mut work = Vec::with_capacity(max_targets);
        while work.len() < max_targets {
            let Some(next) = self.try_claim_work() else { break };
            work.push(next);
        }
        work.sort_unstable_by_key(|work| work.target.physical_order());
        work
    }

    /// Cancels a claimed target when execution consumed its final dependency before the read.
    ///
    /// Returns `true` when the work is stale or no longer belongs to the active parent. A target
    /// canceled this way can be queued again if a later transaction depends on it.
    pub fn cancel_work_if_stale(&self, work: &DowsePrefetchWork) -> bool {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(work.parent_hash) {
            return true;
        }
        let Some(record) = scheduler.targets.get_mut(&work.target) else { return true };
        if record.state != DowsePrefetchTargetState::InFlight || record.version != work.version {
            return true;
        }
        if !record.transaction_priorities.is_empty() {
            return false;
        }

        record.state = DowsePrefetchTargetState::Stale;
        scheduler.in_flight_targets -= 1;
        BuilderMetrics::dowse_prefetch_targets_total(work.target.kind(), "stale_before_read")
            .increment(1);
        BuilderMetrics::dowse_prefetch_reads_in_flight().set(scheduler.in_flight_targets as f64);
        true
    }

    /// Marks one claimed target complete and records whether execution still had use for it.
    pub fn complete_work(&self, work: &DowsePrefetchWork) {
        let mut scheduler = self.scheduler.lock();
        if scheduler.active_parent != Some(work.parent_hash) {
            return;
        }
        let Some(record) = scheduler.targets.get_mut(&work.target) else { return };
        if record.state != DowsePrefetchTargetState::InFlight || record.version != work.version {
            return;
        }
        let outcome = if record.transaction_priorities.is_empty() { "late" } else { "complete" };
        record.state = DowsePrefetchTargetState::Complete;
        scheduler.in_flight_targets -= 1;
        BuilderMetrics::dowse_prefetch_targets_total(work.target.kind(), outcome).increment(1);
        BuilderMetrics::dowse_prefetch_reads_in_flight().set(scheduler.in_flight_targets as f64);
    }

    /// Claims a code hash so only one Dowse worker reads it for this parent.
    pub fn claim_code_hash(&self, parent_hash: B256, code_hash: B256) -> bool {
        let mut scheduler = self.scheduler.lock();
        scheduler.active_parent == Some(parent_hash)
            && scheduler.claimed_code_hashes.insert(code_hash)
    }

    fn push_queue_entry(scheduler: &mut DowsePrefetchSchedulerState, target: DowsePrefetchTarget) {
        let (current, rank, confidence, version) = {
            let Some(record) = scheduler.targets.get(&target) else { return };
            let Some((rank, confidence)) = record.best_priority() else { return };
            let current = scheduler
                .current_transaction
                .is_some_and(|tx_hash| record.transaction_priorities.contains_key(&tx_hash));
            (current, rank, confidence, record.version)
        };
        let sequence = scheduler.next_sequence;
        scheduler.next_sequence = scheduler.next_sequence.wrapping_add(1);
        scheduler.queue.push((
            current,
            Reverse(rank),
            confidence,
            target.kind_priority(),
            Reverse(sequence),
            target,
            version,
        ));
    }

    fn try_claim_work(&self) -> Option<DowsePrefetchWork> {
        let mut scheduler = self.scheduler.lock();
        while let Some((target, version)) = scheduler.urgent_queue.pop_front() {
            let rank = scheduler
                .targets
                .get(&target)
                .and_then(DowsePrefetchTargetRecord::best_priority)
                .map(|(rank, _)| rank)
                .unwrap_or(0);
            if let Some(work) = Self::claim_target(&mut scheduler, target, version, rank) {
                return Some(work);
            }
        }
        while let Some(entry @ (_, Reverse(rank), _, _, _, target, version)) = scheduler.queue.pop()
        {
            if scheduler.builder_rank.is_some_and(|builder_rank| {
                rank > builder_rank.saturating_add(self.max_transaction_distance)
            }) {
                scheduler.queue.push(entry);
                return None;
            }
            if let Some(work) = Self::claim_target(&mut scheduler, target, version, rank) {
                return Some(work);
            }
        }
        None
    }

    fn claim_target(
        scheduler: &mut DowsePrefetchSchedulerState,
        target: DowsePrefetchTarget,
        version: u64,
        rank: usize,
    ) -> Option<DowsePrefetchWork> {
        let queued_at = {
            let record = scheduler.targets.get_mut(&target)?;
            if record.state != DowsePrefetchTargetState::Queued || record.version != version {
                return None;
            }
            record.state = DowsePrefetchTargetState::InFlight;
            record.queued_at
        };
        let distance = scheduler.builder_rank.map(|builder_rank| rank.saturating_sub(builder_rank));
        let parent_hash = scheduler.active_parent.expect("queued work must have a parent");
        let cache = scheduler.active_cache.clone().expect("queued work must have a cache");
        scheduler.queued_targets -= 1;
        scheduler.in_flight_targets += 1;
        BuilderMetrics::dowse_queue_depth().set(scheduler.queued_targets as f64);
        BuilderMetrics::dowse_prefetch_reads_in_flight().set(scheduler.in_flight_targets as f64);
        BuilderMetrics::dowse_prefetch_queue_wait_duration().record(queued_at.elapsed());
        if let Some(distance) = distance {
            BuilderMetrics::dowse_prefetch_builder_distance("dispatch").record(distance as f64);
        }
        Some(DowsePrefetchWork { parent_hash, cache, target, version })
    }
}

impl Default for DowsePrefetchCache {
    fn default() -> Self {
        Self::new(65_536, 4, 2_000)
    }
}

/// One concrete target claimed by a background state-read worker.
#[derive(Debug)]
pub struct DowsePrefetchWork {
    /// State root context against which all reads must execute.
    pub parent_hash: B256,
    /// Cache receiving values read by this work item.
    pub cache: ExecutionCache,
    /// One target claimed from the central priority queue.
    pub target: DowsePrefetchTarget,
    /// Target generation used to reject stale queue entries.
    pub version: u64,
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
        let cache = DowsePrefetchCache::new(
            config.queue_capacity,
            config.max_transaction_distance,
            config.min_confidence_bps,
        );
        Self { client, pool, config, cache }
    }

    /// Spawns persistent workers and the transaction-pool planning loop.
    ///
    /// Returns the cache handle that payload builders should consult for their exact parent hash.
    pub fn spawn(self, executor: Runtime) -> DowsePrefetchCache {
        let shared_cache = self.cache.clone();
        let worker_count = self.config.worker_count;
        let poll_interval = self.config.poll_interval;
        let locality_batch_size = self.config.locality_batch_size;
        let max_transaction_distance = self.config.max_transaction_distance;
        let min_confidence_bps = self.config.min_confidence_bps;

        for _ in 0..self.config.worker_count {
            let client = self.client.clone();
            let cache = self.cache.clone();

            executor.spawn_blocking_task(async move {
                let mut provider: Option<(B256, StateProviderBox)> = None;

                loop {
                    for work in cache.next_work_batch(locality_batch_size).await {
                        if cache.cancel_work_if_stale(&work) {
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
                                    cache.complete_work(&work);
                                    continue;
                                }
                            }
                        }
                        let state_provider = &provider
                            .as_ref()
                            .expect("provider initialized for current Dowse work")
                            .1;

                        if cache.cancel_work_if_stale(&work) {
                            continue;
                        }

                        match work.target {
                            DowsePrefetchTarget::Account(address) => {
                                let account =
                                    match work.cache.get_or_try_insert_account_with(address, || {
                                        state_provider.basic_account(&address)
                                    }) {
                                        Ok(CachedStatus::Cached(account)) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "account",
                                                "cache_hit",
                                            )
                                            .increment(1);
                                            account
                                        }
                                        Ok(CachedStatus::NotCached(account)) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "account", "success",
                                            )
                                            .increment(1);
                                            account
                                        }
                                        Err(error) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "account", "error",
                                            )
                                            .increment(1);
                                            warn!(
                                                %address,
                                                error = %error,
                                                "Dowse account prefetch failed"
                                            );
                                            cache.complete_work(&work);
                                            continue;
                                        }
                                    };

                                if let Some(code_hash) = account.and_then(|info| info.bytecode_hash)
                                    && cache.claim_code_hash(work.parent_hash, code_hash)
                                {
                                    match work.cache.get_or_try_insert_code_with(code_hash, || {
                                        state_provider.bytecode_by_hash(&code_hash)
                                    }) {
                                        Ok(CachedStatus::Cached(_)) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "bytecode",
                                                "cache_hit",
                                            )
                                            .increment(1);
                                        }
                                        Ok(CachedStatus::NotCached(_)) => {
                                            BuilderMetrics::dowse_prefetch_reads_total(
                                                "bytecode", "success",
                                            )
                                            .increment(1);
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
                            DowsePrefetchTarget::Storage { address, slot } => {
                                match work.cache.get_or_try_insert_storage_with(
                                    address,
                                    slot,
                                    || {
                                        state_provider
                                            .storage(address, slot)
                                            .map(Option::unwrap_or_default)
                                    },
                                ) {
                                    Ok(CachedStatus::Cached(_)) => {
                                        BuilderMetrics::dowse_prefetch_reads_total(
                                            "storage",
                                            "cache_hit",
                                        )
                                        .increment(1);
                                    }
                                    Ok(CachedStatus::NotCached(_)) => {
                                        BuilderMetrics::dowse_prefetch_reads_total(
                                            "storage", "success",
                                        )
                                        .increment(1);
                                    }
                                    Err(error) => {
                                        BuilderMetrics::dowse_prefetch_reads_total(
                                            "storage", "error",
                                        )
                                        .increment(1);
                                        warn!(
                                            %address,
                                            %slot,
                                            error = %error,
                                            "Dowse storage prefetch failed"
                                        );
                                    }
                                }
                            }
                        }

                        cache.complete_work(&work);
                        BuilderMetrics::dowse_prefetch_work_duration().record(start.elapsed());
                    }
                }
            });
        }

        let pool = self.pool;
        let config = self.config;
        let cache = self.cache;
        executor.spawn_task(async move {
            let mut timer = tokio::time::interval(config.poll_interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut transaction_events =
                pool.new_transactions_listener_for(TransactionListenerKind::All);
            let planner = PrefetchPlanner::new(
                &config.hints,
                PlanLimits::new(
                    config.max_accounts_per_transaction,
                    config.max_storage_slots_per_transaction,
                ),
            );
            let mut active_parent = None;
            let mut transactions_without_plans = HashSet::new();

            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    transaction = transaction_events.recv() => {
                        if transaction.is_none() {
                            break;
                        }
                        while transaction_events.try_recv().is_ok() {}
                    }
                }
                let parent_hash = pool.block_info().last_seen_block_hash;
                if active_parent != Some(parent_hash) {
                    transactions_without_plans.clear();
                    active_parent = Some(parent_hash);
                    BuilderMetrics::dowse_parent_resets_total().increment(1);
                    cache.cache_for_or_activate(parent_hash, config.cache_size_bytes);
                }

                let mut best_transactions = pool.best_transactions();
                let mut current_transactions = HashSet::new();
                for (rank, transaction) in
                    best_transactions.by_ref().take(config.max_transactions).enumerate()
                {
                    let tx_hash = *transaction.hash();
                    current_transactions.insert(tx_hash);
                    cache.observe_transaction_rank(parent_hash, tx_hash, rank);
                    if cache.contains_transaction(parent_hash, tx_hash) {
                        cache.update_transaction_rank(parent_hash, tx_hash, rank);
                        continue;
                    }
                    if transactions_without_plans.contains(&tx_hash) {
                        continue;
                    }

                    let Some(target) = transaction.to() else {
                        transactions_without_plans.insert(tx_hash);
                        BuilderMetrics::dowse_transactions_total("contract_creation").increment(1);
                        continue;
                    };
                    let Some(plan) =
                        planner.plan(target, transaction.sender(), transaction.transaction.input())
                    else {
                        transactions_without_plans.insert(tx_hash);
                        BuilderMetrics::dowse_transactions_total("no_hints").increment(1);
                        continue;
                    };

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
                        transactions_without_plans.insert(tx_hash);
                        BuilderMetrics::dowse_transactions_total("empty_plan").increment(1);
                        continue;
                    }

                    cache.submit_plan(parent_hash, tx_hash, rank, plan);
                    BuilderMetrics::dowse_transactions_total("planned").increment(1);
                }
                cache.retain_transactions(parent_hash, &current_transactions);
                transactions_without_plans.retain(|tx_hash| current_transactions.contains(tx_hash));
            }
        });

        info!(
            workers = worker_count,
            max_transaction_distance,
            locality_batch_size,
            min_confidence_bps,
            poll_interval_ms = poll_interval.as_millis(),
            "Dowse event-driven transaction-pool prefetcher started"
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

        let same = cache.cache_for_or_activate(parent, 1_000_000);
        assert!(matches!(
            same.get_or_try_insert_storage_with(address, slot, || Ok::<_, eyre::Error>(
                U256::ZERO
            )),
            Ok(CachedStatus::Cached(cached)) if cached == value
        ));
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
    fn scheduler_dispatches_nearest_transaction_first() {
        let parent = B256::random();
        let farther = Address::random();
        let nearer = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            B256::repeat_byte(1),
            5,
            PrefetchPlan { accounts: vec![farther], ..Default::default() },
        );
        cache.submit_plan(
            parent,
            B256::repeat_byte(2),
            1,
            PrefetchPlan { accounts: vec![nearer], ..Default::default() },
        );

        let work = cache.try_claim_work().expect("nearest target should be queued");
        assert_eq!(work.target, DowsePrefetchTarget::Account(nearer));
    }

    #[test]
    fn scheduler_admits_and_prioritizes_hint_confidence() {
        let parent = B256::random();
        let low = Address::random();
        let medium = Address::random();
        let high = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 5_000);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            B256::random(),
            0,
            PrefetchPlan {
                accounts: vec![low, medium, high],
                account_confidence: vec![0.1, 0.6, 0.9],
                ..Default::default()
            },
        );

        let first = cache.try_claim_work().expect("high-confidence target should be queued");
        assert_eq!(first.target, DowsePrefetchTarget::Account(high));
        cache.complete_work(&first);
        let second = cache.try_claim_work().expect("medium-confidence target should be queued");
        assert_eq!(second.target, DowsePrefetchTarget::Account(medium));
        cache.complete_work(&second);
        assert!(cache.try_claim_work().is_none(), "low-confidence target must be omitted");
    }

    #[test]
    fn builder_cursor_promotes_exact_transaction() {
        let parent = B256::random();
        let nearer_tx = B256::repeat_byte(1);
        let farther_tx = B256::repeat_byte(2);
        let nearer = Address::random();
        let farther = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            nearer_tx,
            0,
            PrefetchPlan { accounts: vec![nearer], ..Default::default() },
        );
        cache.submit_plan(
            parent,
            farther_tx,
            10,
            PrefetchPlan { accounts: vec![farther], ..Default::default() },
        );

        cache.start_transaction(parent, farther_tx);
        let work = cache.try_claim_work().expect("promoted target should be queued");
        assert_eq!(work.target, DowsePrefetchTarget::Account(farther));
    }

    #[test]
    fn duplicate_target_is_dispatched_once_for_all_transactions() {
        let parent = B256::random();
        let first_tx = B256::repeat_byte(1);
        let second_tx = B256::repeat_byte(2);
        let address = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        for (tx_hash, rank) in [(first_tx, 0), (second_tx, 1)] {
            cache.submit_plan(
                parent,
                tx_hash,
                rank,
                PrefetchPlan { accounts: vec![address], ..Default::default() },
            );
        }

        cache.finish_transaction(parent, first_tx);
        let work = cache.try_claim_work().expect("shared target should remain queued");
        assert!(!cache.cancel_work_if_stale(&work));
        assert_eq!(work.target, DowsePrefetchTarget::Account(address));
        cache.complete_work(&work);
        assert!(cache.try_claim_work().is_none(), "duplicate target must not be dispatched twice");
    }

    #[test]
    fn consumed_transaction_cancels_queued_targets() {
        let parent = B256::random();
        let tx_hash = B256::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            tx_hash,
            0,
            PrefetchPlan { accounts: vec![Address::random()], ..Default::default() },
        );

        cache.finish_transaction(parent, tx_hash);
        assert!(cache.try_claim_work().is_none(), "stale target must not reach a worker");
    }

    #[test]
    fn stale_target_can_be_requeued_for_a_new_transaction() {
        let parent = B256::random();
        let address = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            B256::repeat_byte(1),
            0,
            PrefetchPlan { accounts: vec![address], ..Default::default() },
        );
        cache.finish_transaction(parent, B256::repeat_byte(1));
        cache.submit_plan(
            parent,
            B256::repeat_byte(2),
            1,
            PrefetchPlan { accounts: vec![address], ..Default::default() },
        );

        let work = cache.try_next_work().expect("new dependency should revive stale target");
        assert_eq!(work.target, DowsePrefetchTarget::Account(address));
    }

    #[test]
    fn canceled_in_flight_target_can_be_requeued() {
        let parent = B256::random();
        let first_tx = B256::repeat_byte(1);
        let second_tx = B256::repeat_byte(2);
        let address = Address::random();
        let cache = DowsePrefetchCache::new(8, 8, 0);
        cache.activate(parent, 1_000_000);
        cache.submit_plan(
            parent,
            first_tx,
            0,
            PrefetchPlan { accounts: vec![address], ..Default::default() },
        );
        let stale = cache.try_next_work().expect("first dependency should be queued");
        cache.finish_transaction(parent, first_tx);
        assert!(cache.cancel_work_if_stale(&stale));

        cache.submit_plan(
            parent,
            second_tx,
            1,
            PrefetchPlan { accounts: vec![address], ..Default::default() },
        );
        let revived = cache.try_next_work().expect("new dependency should revive canceled work");
        assert_eq!(revived.target, DowsePrefetchTarget::Account(address));
    }

    #[test]
    fn scheduler_stays_within_builder_distance() {
        let parent = B256::random();
        let current_tx = B256::repeat_byte(1);
        let near_tx = B256::repeat_byte(2);
        let far_tx = B256::repeat_byte(3);
        let near = Address::random();
        let far = Address::random();
        let cache = DowsePrefetchCache::new(8, 2, 0);
        cache.activate(parent, 1_000_000);
        cache.observe_transaction_rank(parent, current_tx, 0);
        cache.submit_plan(
            parent,
            near_tx,
            2,
            PrefetchPlan { accounts: vec![near], ..Default::default() },
        );
        cache.submit_plan(
            parent,
            far_tx,
            3,
            PrefetchPlan { accounts: vec![far], ..Default::default() },
        );
        cache.start_transaction(parent, current_tx);

        let work = cache.try_next_work().expect("near target should be admitted");
        assert_eq!(work.target, DowsePrefetchTarget::Account(near));
        cache.complete_work(&work);
        assert!(cache.try_next_work().is_none(), "far target must wait for the cursor");

        cache.start_transaction(parent, near_tx);
        let work = cache.try_next_work().expect("cursor advance should admit far target");
        assert_eq!(work.target, DowsePrefetchTarget::Account(far));
    }

    #[test]
    fn ab_test_alternates_cache_use_by_parent_hash() {
        let mut config = DowseConfig {
            hints: Arc::new(HintTable::default()),
            ab_test: false,
            worker_count: 1,
            queue_capacity: 1,
            max_transaction_distance: 1,
            locality_batch_size: 1,
            min_confidence_bps: 0,
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
