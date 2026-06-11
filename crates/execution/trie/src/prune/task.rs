use std::sync::Arc;

use reth_provider::BlockHashReader;
use reth_tasks::shutdown::GracefulShutdown;
use tokio::{
    time,
    time::{Duration, MissedTickBehavior},
};
use tracing::{info, warn};

use crate::{BaseProofsStorage, BaseProofsStore, prune::BaseProofStoragePruner};

/// Number of blocks pruned per MDBX write transaction.
///
/// Each batch is its own write tx, so the per-tx overhead (page allocation, freelist mgmt,
/// fsync at commit) amortizes over this many blocks. The previous value of 200 caused tx
/// overhead to dominate catch-up pruning runs; 2000 amortizes the fixed cost ~10x while
/// keeping per-tx dirty-page sets and free-page reclamation lag bounded.
const PRUNE_BATCH_SIZE: u64 = 2000;

/// Periodic pruner task: constructs the pruner and runs it every interval.
#[derive(Debug)]
pub struct BaseProofStoragePrunerTask<P, H> {
    pruner: BaseProofStoragePruner<P, H>,
    retention_blocks: u64,
    task_run_interval: Duration,
}

impl<P, H> BaseProofStoragePrunerTask<P, H>
where
    P: BaseProofsStore + Send + Sync + 'static,
    H: BlockHashReader + Send + Sync + 'static,
{
    /// Initialize a new [`BaseProofStoragePrunerTask`]
    pub const fn new(
        provider: BaseProofsStorage<P>,
        hash_reader: H,
        retention_blocks: u64,
        task_run_interval: Duration,
    ) -> Self {
        let pruner =
            BaseProofStoragePruner::new(provider, hash_reader, retention_blocks, PRUNE_BATCH_SIZE);
        Self { pruner, retention_blocks, task_run_interval }
    }

    /// Run forever (until `cancel`), executing one prune pass per `task_run_interval`.
    pub async fn run(self, mut signal: GracefulShutdown) {
        info!(
            target: "trie::pruner_task",
            retention_blocks = self.retention_blocks,
            interval_secs = self.task_run_interval.as_secs(),
            "Starting pruner task"
        );

        let mut interval = time::interval(self.task_run_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let pruner = Arc::new(self.pruner);
        loop {
            tokio::select! {
                _ = &mut signal => {
                    info!(target: "trie::pruner_task", "Pruner task cancelled; exiting");
                    break;
                }
                _ = interval.tick() => {
                    let pruner = Arc::clone(&pruner);
                    if let Err(e) = tokio::task::spawn_blocking(move || pruner.run()).await {
                        warn!(target: "trie::pruner_task", err=%e, "Pruner blocking task failed");
                    }
                }
            }
        }

        info!(target: "trie::pruner_task", "Pruner task stopped");
    }
}
