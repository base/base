//! Txpool-driven state prewarming and immutable snapshot publication.

use base_common_types_chain::BaseTxEnvelope;
use base_execution_evm_blocks::BaseEvmConfig;
mod control;
mod worker;

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::{Address, B256};
use base_common_types_chain::transaction::Recovered;
use base_execution_state_provider::{
    BlockNumReader, DatabaseProviderFactory, PruneCheckpointReader, StageCheckpointReader,
    StorageSettingsCache, TryIntoHistoricalStateProvider,
};

use self::control::Control;
use crate::tree::{StateProviderBuilder, TxPoolPrewarmCacheSnapshot};

/// Coordinates a long-lived worker and the latest completed immutable snapshot.
pub(crate) struct Handle {
    control: Arc<Control<Job>>,
}

impl Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").field("control", &self.control).finish()
    }
}

impl Handle {
    /// Spawns the long-lived worker, which owns its mutable read cache and starts a fresh one for
    /// each new head.
    pub(crate) fn spawn(
        runtime: &base_common_runtime::Runtime,
        source: Arc<dyn Source>,
        evm_config: BaseEvmConfig,
    ) -> Self {
        let (control, commands) = Control::new();
        let publication = control.publication();
        runtime.spawn_critical_os_thread("txpool-prewarm", "txpool prewarm worker", async move {
            worker::Worker::new(commands, publication, source, evm_config).run()
        });
        Self { control }
    }

    /// Pauses speculative work.
    ///
    /// Returns a guard that will resume the worker when dropped. There could be multiple
    /// outstanding guards, in which case the worker will not resume until all guards are dropped.
    ///
    /// Pausing is asynchronous and never blocks the caller: the worker observes it between
    /// transactions, so speculative work may overlap the guard's scope by at most one
    /// transaction.
    pub(crate) fn pause(&self) -> impl Drop + Send + 'static {
        self.control.pause()
    }

    /// Returns the latest fully published snapshot for `parent_hash`, or `None` if no snapshot is
    /// available for that hash.
    pub(crate) fn snapshot(&self, parent_hash: B256) -> Option<TxPoolPrewarmCacheSnapshot> {
        self.control.snapshot(parent_hash)
    }

    /// Starts continuous warming for the latest canonical head.
    pub(crate) fn start(
        &self,
        parent_hash: B256,
        evm_env: base_execution_evm_runtime::EvmEnv<
            base_execution_evm_runtime::BaseSpecId,
            base_execution_evm_runtime::BlockEnv,
        >,
        provider_builder: StateProviderBuilder,
    ) {
        self.control.start(parent_hash, Job { evm_env, provider_builder });
    }
}

/// A live, forward-only view of the pool's best transactions for one canonical parent.
///
/// Returning [`None`](Iterator::next) only means no transaction is currently ready. The same
/// iterator can yield transactions that become pending later.
pub type Transactions = Box<dyn Iterator<Item = Transaction> + Send>;

/// A transaction selected from the txpool for cache-only prewarming.
#[derive(Debug, Clone)]
pub struct Transaction {
    /// Transaction hash.
    pub hash: B256,
    /// Recovered sender.
    pub sender: Address,
    /// Recovered consensus transaction.
    pub transaction: Recovered<BaseTxEnvelope>,
}

/// Source of txpool transactions for best-effort cache prewarming.
pub trait Source: Send + Sync + Debug {
    /// Opens a live best-transactions iterator for `parent_hash`.
    ///
    /// The worker opens this once per canonical parent and retains it across empty polls, snapshot
    /// publications, and validation pauses. Sources should return [`None`] if they are not yet
    /// tracking `parent_hash`.
    fn best_transactions(&self, parent_hash: B256) -> Option<Transactions>;
}

/// A request to warm txpool transactions against one fully validated parent state.
struct Job {
    evm_env: base_execution_evm_runtime::EvmEnv<
        base_execution_evm_runtime::BaseSpecId,
        base_execution_evm_runtime::BlockEnv,
    >,
    provider_builder: StateProviderBuilder,
}
