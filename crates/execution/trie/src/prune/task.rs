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
    pruner: Arc<BaseProofStoragePruner<P, H>>,
    min_block_interval: u64,
    task_run_interval: Duration,
}

impl<P, H> BaseProofStoragePrunerTask<P, H>
where
    P: BaseProofsStore + Send + Sync + 'static,
    H: BlockHashReader + Send + Sync + 'static,
{
    /// Initialize a new [`BaseProofStoragePrunerTask`]
    pub fn new(
        provider: BaseProofsStorage<P>,
        hash_reader: H,
        min_block_interval: u64,
        task_run_interval: Duration,
    ) -> Self {
        let pruner = Arc::new(BaseProofStoragePruner::new(
            provider,
            hash_reader,
            min_block_interval,
            PRUNE_BATCH_SIZE,
        ));
        Self { pruner, min_block_interval, task_run_interval }
    }

    /// Run forever (until `cancel`), executing one prune pass per `task_run_interval`.
    pub async fn run(self, mut signal: GracefulShutdown) {
        info!(
            target: "trie::pruner_task",
            min_block_interval = self.min_block_interval,
            interval_secs = self.task_run_interval.as_secs(),
            "Starting pruner task"
        );

        let mut interval = time::interval(self.task_run_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = &mut signal => {
                    info!(target: "trie::pruner_task", "Pruner task cancelled; exiting");
                    break;
                }
                _ = interval.tick() => {
                    // `pruner.run()` performs blocking MDBX read/write transactions; offload
                    // to a blocking worker so the tokio runtime stays responsive (and the
                    // shutdown branch above can preempt on the next tick).
                    let pruner = Arc::clone(&self.pruner);
                    if let Err(e) = tokio::task::spawn_blocking(move || pruner.run()).await {
                        warn!(target: "trie::pruner_task", err=%e, "Pruner blocking task failed");
                    }
                }
            }
        }

        info!(target: "trie::pruner_task", "Pruner task stopped");
    }
}
