//! Speculative txpool prewarming for the flashblocks builder.
//!
//! Block building executes the pool's best transactions single-threaded, stalling on the
//! synchronous state IO (MDBX + trie reads) each execution incurs. This module runs a best-effort
//! prewarmer that speculatively executes those same transactions against the canonical head state
//! on a blocking thread, recording every state read into a [`CachedReads`]. It periodically freezes
//! that cache into an immutable [`TxPoolPrewarmCacheSnapshot`] keyed by parent hash.
//!
//! The builder consults the snapshot as the first tier of its [`CachedStateProvider`](
//! reth_execution_cache::CachedStateProvider) (see [`PrewarmHandle::snapshot_for`]), so reads the
//! prewarmer already performed become cache hits instead of disk stalls, flattening the IO wait
//! that would otherwise be fully incurred during the single-threaded build.
//!
//! Warming runs continuously against the current canonical head: by the time a forkchoice update
//! asks the builder to build the next block on that head, a hot snapshot is already available. A
//! new canonical head cancels the previous warming pass and starts a fresh one. Warming is
//! out-of-context by design — nonce, balance, and base-fee checks are disabled so viability never
//! gates which state is warmed — and its execution results are discarded; only the reads are kept.
//!
//! This mirrors reth's engine-side txpool prewarmer (`reth`'s `txpool_prewarm` worker), whose
//! orchestration types are `pub(crate)` and so cannot be reused directly, while reusing reth's
//! public [`TxPoolPrewarmCacheSnapshot`] and snapshot cache tier.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_evm::{Evm, EvmEnv};
use alloy_primitives::B256;
use base_common_consensus::BasePrimitives;
use base_common_evm::BaseSpecId;
use base_execution_evm::BaseEvmConfig;
use futures::StreamExt;
use parking_lot::RwLock;
use reth_evm::ConfigureEvm;
use reth_execution_cache::TxPoolPrewarmCacheSnapshot;
use reth_provider::{CanonStateNotificationStream, StateProviderFactory};
use reth_revm::{State, cached::CachedReads, context::Block, database::StateProviderDatabase};
use reth_tasks::Runtime;
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction};
use tracing::{debug, trace};

use crate::traits::{ClientBounds, PoolBounds};

/// Maximum time a single warming batch executes before releasing the state provider and publishing.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on how long a single warming pass keeps warming one parent.
///
/// A new canonical head normally cancels the pass well before this; the cap only bounds a stalled
/// or silent canonical stream so a blocking worker cannot spin forever on stale state.
const MAX_WARM_LIFETIME: Duration = Duration::from_secs(4);

/// A cheap, cloneable handle onto the freshest published prewarm snapshot.
///
/// The builder holds one of these and calls [`Self::snapshot_for`] at the top of each build to seed
/// its cache. Cloning shares the same underlying slot the [`TxPoolPrewarmer`] publishes into.
#[derive(Clone, Debug)]
pub struct PrewarmHandle {
    /// The latest published snapshot, replaced in place by the warming worker.
    latest: Arc<RwLock<Option<TxPoolPrewarmCacheSnapshot>>>,
}

impl PrewarmHandle {
    /// Returns a handle that never yields a snapshot.
    ///
    /// Used when prewarming is disabled, so the builder degrades to its uncached behavior.
    pub fn noop() -> Self {
        Self { latest: Arc::new(RwLock::new(None)) }
    }

    /// Returns the latest snapshot, but only when it was warmed against `parent_hash`.
    ///
    /// A snapshot warmed against a different parent describes the wrong state and is discarded, so
    /// the builder falls back to its execution cache and disk.
    pub fn snapshot_for(&self, parent_hash: B256) -> Option<TxPoolPrewarmCacheSnapshot> {
        self.latest.read().clone().filter(|snapshot| snapshot.parent_hash() == parent_hash)
    }
}

/// Best-effort txpool state prewarmer for the flashblocks builder.
///
/// Drives a warming worker off canonical head notifications; the worker executes the pool's best
/// transactions against the head state and publishes [`TxPoolPrewarmCacheSnapshot`]s that the
/// builder consumes through a [`PrewarmHandle`].
///
/// Cloning is cheap — every field is an [`Arc`] or an `Arc`-backed handle — and each warming pass
/// runs on a clone so it can be moved onto a blocking thread.
#[derive(Clone)]
pub struct TxPoolPrewarmer<Pool, Client> {
    /// The transaction pool speculative transactions are drawn from.
    pool: Pool,
    /// The client used to open a state provider for the parent being warmed.
    client: Client,
    /// Configures the EVM used for speculative execution.
    evm_config: BaseEvmConfig,
    /// Runtime used to spawn the async driver and blocking warming passes.
    runtime: Runtime,
    /// The slot the worker publishes snapshots into and [`PrewarmHandle`]s read from.
    latest: Arc<RwLock<Option<TxPoolPrewarmCacheSnapshot>>>,
    /// Monotonic head counter. Each new canonical head bumps it; a warming pass runs and publishes
    /// only while it still holds the current epoch, so a superseded pass can neither keep working
    /// nor overwrite a fresher snapshot.
    epoch: Arc<AtomicU64>,
}

impl<Pool, Client> std::fmt::Debug for TxPoolPrewarmer<Pool, Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxPoolPrewarmer").finish_non_exhaustive()
    }
}

impl<Pool, Client> TxPoolPrewarmer<Pool, Client>
where
    Pool: PoolBounds,
    Client: ClientBounds + StateProviderFactory + Send + Sync + 'static,
{
    /// Creates a new prewarmer. Call [`Self::spawn`] to start warming.
    pub fn new(pool: Pool, client: Client, evm_config: BaseEvmConfig, runtime: Runtime) -> Self {
        Self {
            pool,
            client,
            evm_config,
            runtime,
            latest: Arc::new(RwLock::new(None)),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns a handle onto the snapshots this prewarmer publishes.
    ///
    /// The handle can be obtained before [`Self::spawn`] and shared with the builder.
    pub fn handle(&self) -> PrewarmHandle {
        PrewarmHandle { latest: Arc::clone(&self.latest) }
    }

    /// Spawns the async driver that warms each new canonical head until the stream ends.
    pub fn spawn(self, stream: CanonStateNotificationStream<BasePrimitives>) {
        let runtime = self.runtime.clone();
        runtime.spawn_task(self.drive(stream));
    }

    /// Consumes canonical head notifications, restarting the warming pass for each new head.
    async fn drive(self, mut stream: CanonStateNotificationStream<BasePrimitives>) {
        while let Some(notification) = stream.next().await {
            // `committed()` yields the new canonical chain for both commits and reorgs, so its tip
            // is always the head the builder will build on next, including immediately after a
            // reorg. We read only the tip's absolute state (never the execution-outcome diff), so
            // unlike `state_diff_maintain` reorgs need no special-casing here.
            let committed = notification.committed();
            let tip = committed.tip();
            let parent_hash = tip.hash();
            let evm_env = match self.evm_config.evm_env(tip.header()) {
                Ok(evm_env) => evm_env,
                Err(error) => {
                    debug!(
                        target: "payload_builder",
                        parent_hash = %parent_hash,
                        ?error,
                        "failed to build prewarm evm env",
                    );
                    continue;
                }
            };

            // Claim the next epoch. This supersedes any in-flight pass: it observes the bump and
            // stops, and can no longer publish over the snapshot this new pass produces.
            let epoch = self.epoch.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

            let worker = self.clone();
            self.runtime.spawn_blocking(move || worker.warm(parent_hash, evm_env, epoch));
        }

        // Supersede the final pass so it stops once the stream ends.
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Speculatively executes the pool's best transactions against `parent_hash`'s state,
    /// publishing a fresh snapshot each time the recorded reads grow.
    ///
    /// Runs until a newer head supersedes this `epoch`, the pass exceeds [`MAX_WARM_LIFETIME`], or
    /// the parent state becomes unavailable.
    fn warm(&self, parent_hash: B256, evm_env: EvmEnv<BaseSpecId>, epoch: u64) {
        let attributes = BestTransactionsAttributes::new(
            evm_env.block_env.basefee,
            evm_env.block_env.blob_gasprice().map(|price| price as u64),
        );

        let lifetime_deadline = Instant::now() + MAX_WARM_LIFETIME;
        let mut cache = CachedReads::default();
        // Cache entry counts as of the last publication; the cache only grows, so a change means it
        // holds unpublished reads worth republishing.
        let mut published = (0usize, 0usize, 0usize);
        let mut best = self.pool.best_transactions_with_attributes(attributes);

        while self.epoch.load(Ordering::Relaxed) == epoch && Instant::now() < lifetime_deadline {
            // Reopen the state provider each batch rather than holding one for the whole pass: a
            // pass can span up to `MAX_WARM_LIFETIME`, and keeping a single MDBX read transaction
            // open that long would pin the freelist. `CachedReads` already serves prior reads, so
            // only genuine misses reach the freshly opened provider.
            let state_provider = match self.client.state_by_block_hash(parent_hash) {
                Ok(state_provider) => state_provider,
                Err(error) => {
                    trace!(
                        target: "payload_builder",
                        parent_hash = %parent_hash,
                        ?error,
                        "no state available for prewarm parent",
                    );
                    return;
                }
            };

            let mut state = State::builder()
                .with_database(cache.as_db_mut(StateProviderDatabase::new(state_provider)))
                .build();

            // Warming is out of context by design: transaction viability is the pool's business,
            // so nonce, balance, and (one-block-stale) base-fee checks must not gate which state
            // gets warmed.
            let mut env = evm_env.clone();
            env.cfg_env.disable_nonce_check = true;
            env.cfg_env.disable_balance_check = true;
            env.cfg_env.disable_base_fee = true;
            let mut evm = self.evm_config.evm_with_env(&mut state, env);

            let batch_deadline = Instant::now() + REFRESH_INTERVAL;
            let mut exhausted = false;
            while self.epoch.load(Ordering::Relaxed) == epoch && Instant::now() < batch_deadline {
                let Some(transaction) = best.next() else {
                    exhausted = true;
                    break;
                };
                let recovered = transaction.transaction.clone_into_consensus();
                if let Err(error) = evm.transact(&recovered) {
                    trace!(
                        target: "payload_builder",
                        tx_hash = ?transaction.hash(),
                        ?error,
                        "speculative prewarm execution failed",
                    );
                }
            }

            // Drop the EVM and state so the mutable borrow of `cache` is released before it is read.
            drop(evm);
            drop(state);

            let counts = (
                cache.accounts.len(),
                cache.accounts.values().map(|account| account.storage.len()).sum::<usize>(),
                cache.contracts.len(),
            );
            if counts != published && self.epoch.load(Ordering::Relaxed) == epoch {
                let mut latest = self.latest.write();
                // Re-check under the write lock so a superseded pass can never overwrite the
                // snapshot the current pass published for the new head.
                if self.epoch.load(Ordering::Relaxed) == epoch {
                    *latest =
                        Some(TxPoolPrewarmCacheSnapshot::new(parent_hash, Arc::new(cache.clone())));
                    published = counts;
                    let (accounts, storage, bytecodes) = counts;
                    debug!(
                        target: "payload_builder",
                        parent_hash = %parent_hash,
                        accounts,
                        storage,
                        bytecodes,
                        "published txpool prewarm snapshot",
                    );
                }
            }

            if exhausted {
                // Pool is drained for now; wait for maintenance to surface more pending
                // transactions, then reopen a fresh iterator against the same parent.
                std::thread::sleep(REFRESH_INTERVAL);
                best = self.pool.best_transactions_with_attributes(attributes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use reth_execution_cache::TxPoolPrewarmCacheSnapshot;
    use reth_revm::cached::CachedReads;

    use super::PrewarmHandle;

    #[test]
    fn noop_handle_never_yields_a_snapshot() {
        let handle = PrewarmHandle::noop();
        assert!(handle.snapshot_for(B256::repeat_byte(0x01)).is_none());
    }

    #[test]
    fn snapshot_is_returned_only_for_its_parent() {
        let handle = PrewarmHandle::noop();
        let parent = B256::repeat_byte(0x01);
        *handle.latest.write() =
            Some(TxPoolPrewarmCacheSnapshot::new(parent, Arc::new(CachedReads::default())));

        assert!(handle.snapshot_for(parent).is_some(), "matching parent hits");
        assert!(
            handle.snapshot_for(B256::repeat_byte(0x02)).is_none(),
            "a snapshot for another parent is discarded",
        );
    }
}
