//! Transaction-simulation warming jobs and scheduling state.

use std::sync::Arc;

use alloy_primitives::{TxHash, map::HashSet};
use reth_execution_cache::CachedStateProvider;
use reth_storage_api::StateProviderBox;

/// Simulates one transaction through a cache-filling provider, discarding all output.
///
/// Erased so the queue and worker pool stay free of the builder's EVM generics: the
/// closure is built on the build thread, where the concrete `ConfigureEvm` type is known.
pub type SimulateFn = dyn Fn(&CachedStateProvider<StateProviderBox>) + Send + Sync;

/// One transaction-simulation prewarm job.
///
/// Running the job executes the transaction against a throwaway state overlay layered on
/// the job's cache-filling provider, warming the transaction's entire EVM read set
/// (accounts, storage, and bytecode) rather than only its declared predicate keys. All
/// execution output is discarded: simulation never contributes to inclusion, ordering,
/// gas accounting, or the payload.
pub struct SimJob {
    /// Simulates the transaction, discarding its result.
    pub simulate: Box<SimulateFn>,
    /// The simulated transaction's hash, used to deduplicate scheduling and to detect
    /// the build loop overtaking an unfinished simulation.
    pub tx_hash: TxHash,
}

impl std::fmt::Debug for SimJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimJob").field("tx_hash", &self.tx_hash).finish_non_exhaustive()
    }
}

/// Builds the [`SimJob`] for one pooled transaction.
///
/// Returns `None` when no simulation can be prepared for the transaction (for example the
/// transaction cannot be converted into an EVM transaction environment). Built on the
/// build thread, where the concrete `ConfigureEvm` type is known.
pub type SimJobFactory<T> = dyn Fn(&T) -> Option<SimJob> + Send + Sync;

/// Build-side transaction-simulation warming setup for the lookahead adapter.
pub struct SimSetup<T> {
    /// Builds the simulation job for one lookahead transaction.
    pub factory: Arc<SimJobFactory<T>>,
    /// Maximum simulations outstanding (queued or in flight) at any time.
    pub lookahead: usize,
}
impl<T> Clone for SimSetup<T> {
    fn clone(&self) -> Self {
        Self { factory: Arc::clone(&self.factory), lookahead: self.lookahead }
    }
}

impl<T> std::fmt::Debug for SimSetup<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimSetup").field("lookahead", &self.lookahead).finish_non_exhaustive()
    }
}

/// Simulation scheduling state for one build.
#[derive(Debug, Default)]
pub struct SimSchedulerState {
    /// Every transaction scheduled for simulation this build, for deduplication.
    pub scheduled: HashSet<TxHash>,
    /// Scheduled simulations that have not finished executing on a worker.
    pub pending: HashSet<TxHash>,
}
